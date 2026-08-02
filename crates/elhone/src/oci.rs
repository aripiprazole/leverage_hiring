use std::{
    collections::BTreeMap,
    fmt::{self, Display},
    str::FromStr,
    sync::Once,
};

use axum::{Extension, Json, extract::rejection::JsonRejection};
use futures::TryStreamExt;
use reqwest::{
    Client, Response, StatusCode, Url,
    header::{ACCEPT, CONTENT_TYPE, HeaderMap, WWW_AUTHENTICATE},
    redirect::{Attempt, Policy},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};

use super::ApiError;

#[derive(Debug, Clone, Copy)]
enum TransportPolicy {
    HttpsOnly,
    #[cfg(test)]
    HttpLoopback,
}

impl TransportPolicy {
    #[tracing::instrument(err)]
    fn parse<'a>(self, url: &'a Url) -> Result<OutboundUrl<'a>, OciError> {
        self.allows(url)
            .then_some(OutboundUrl(url))
            .ok_or_else(|| OciError::InvalidInput(format!("OCI URL must use HTTPS: {url}")))
    }

    #[tracing::instrument]
    fn allows(self, url: &Url) -> bool {
        if url.scheme() == "https" {
            return true;
        }

        #[cfg(test)]
        if matches!(self, Self::HttpLoopback) && url.scheme() == "http" {
            return matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
        }

        false
    }
}

#[derive(Debug, Clone, Copy)]
struct OutboundUrl<'a>(&'a Url);

impl AsRef<Url> for OutboundUrl<'_> {
    fn as_ref(&self) -> &Url {
        self.0
    }
}

impl Display for OutboundUrl<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone)]
pub struct OciFetcher {
    client: Client,
    transport: TransportPolicy,
    platform: Option<HostPlatform>,
}

impl Default for OciFetcher {
    #[tracing::instrument]
    fn default() -> Self {
        Self::new(TransportPolicy::HttpsOnly, None)
    }
}

impl OciFetcher {
    #[tracing::instrument]
    fn new(transport: TransportPolicy, platform: Option<HostPlatform>) -> Self {
        install_ring_crypto_provider();
        let client = Client::builder()
            .redirect(Policy::custom(move |attempt| {
                if transport.allows(attempt.url()) {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            .expect("static OCI HTTP client configuration should be valid");

        Self {
            client,
            transport,
            platform,
        }
    }

    #[cfg(test)]
    #[tracing::instrument]
    fn for_test(platform: HostPlatform) -> Self {
        Self::new(TransportPolicy::HttpLoopback, Some(platform))
    }

    #[tracing::instrument(skip(self), fields(url = %location), err)]
    async fn fetch(&self, location: RegistryLoc) -> Result<api_schema::Run, OciError> {
        let initial_url = self.transport.parse(&location.manifest)?;
        tracing::warn!(
            url = %location.manifest,
            host = location.manifest.host_str().unwrap_or_default(),
            "fetching OCI data from a caller-supplied registry URL"
        );

        let host = match self.platform {
            Some(ref platform) => platform.clone(),
            None => HostPlatform::current()?,
        };
        let mut session = FetchSession::new(&self.client, self.transport);
        let initial = session.fetch_manifest(initial_url, None).await?;

        let (index, selected_platform, manifest) = match initial {
            FetchedManifest::Index {
                document,
                digest,
                media_type,
            } => {
                let selected = host.select_platform(&document.manifests)?;
                let manifest_url = location.manifest_url(&selected.digest)?;
                let manifest_url = self.transport.parse(&manifest_url)?;
                let fetched = session
                    .fetch_manifest(manifest_url, Some(&selected))
                    .await?;
                let FetchedManifest::Manifest(fetched) = fetched else {
                    return Err(OciError::InvalidDocument(
                        "selected platform descriptor resolved to another image index".to_owned(),
                    ));
                };
                (
                    Some(api_schema::Document {
                        schema_version: document.schema_version,
                        media_kind,
                        digest,
                        annotations: document.annotations,
                    }),
                    selected.platform,
                    fetched,
                )
            }
            FetchedManifest::Manifest(fetched) => (None, None, fetched),
        };

        let config_url = location.blob_url(&manifest.document.config.digest)?;
        let config = session
            .fetch_config(
                self.transport.parse(&config_url)?,
                &manifest.document.config,
            )
            .await?;

        let platform = config.platform(selected_platform);
        let image = config.response();
        let mut layers = Vec::with_capacity(manifest.document.layers.len());
        for descriptor in &manifest.document.layers {
            let layer_url = location.blob_url(&descriptor.digest)?;
            let layer_url = self.transport.parse(&layer_url)?;
            layers.push(session.fetch_layer(layer_url, descriptor).await?);
        }

        Ok(api_schema::Run {
            url: location.manifest.to_string(),
            platform,
            index,
            manifest: api_schema::Manifest {
                schema_version: manifest.document.schema_version,
                media_kind: manifest.kind,
                digest: manifest.digest,
                annotations: manifest.document.annotations,
                config: api_schema::Descriptor {
                    media_kind: manifest.document.config.media_kind,
                    digest: manifest.document.config.digest,
                    size: manifest.document.config.size,
                },
            },
            image,
            rootfs: config.rootfs,
            history: config.history,
            layers,
        })
    }
}

fn install_ring_crypto_provider() {
    static INSTALL_RING_CRYPTO_PROVIDER: Once = Once::new();

    INSTALL_RING_CRYPTO_PROVIDER.call_once(|| {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("ring should be the only installed rustls crypto provider");
    });
}

#[tracing::instrument(skip(fetcher, input), err)]
pub async fn run(
    Extension(fetcher): Extension<OciFetcher>,
    input: Result<Json<api_schema::RunInput>, JsonRejection>,
) -> Result<Json<api_schema::Run>, ApiError> {
    let Json(input) = input.map_err(|error| ApiError::UnprocessableEntity(error.body_text()))?;
    fetcher
        .fetch(input.url)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

struct FetchSession<'a> {
    client: &'a Client,
    transport: TransportPolicy,
    bearer_token: Option<String>,
}

impl<'a> FetchSession<'a> {
    #[tracing::instrument(skip(client))]
    fn new(client: &'a Client, transport: TransportPolicy) -> Self {
        Self {
            client,
            transport,
            bearer_token: None,
        }
    }

    #[tracing::instrument(skip(self, expected), fields(url = %url), err)]
    async fn fetch_manifest(
        &mut self,
        url: OutboundUrl<'_>,
        expected: Option<&descriptor::Manifest>,
    ) -> Result<FetchedManifest, OciError> {
        let fetched = self
            .fetch_bytes(
                url,
                media::ACCEPT,
                expected.map(|descriptor| descriptor.expectation()),
            )
            .await?;
        let document =
            serde_json::from_slice::<ManifestDocument>(&fetched.body).map_err(|source| {
                OciError::Json {
                    url: url.to_string(),
                    source,
                }
            })?;
        match document {
            ManifestDocument::Index(document) => {
                let media_type = resolve_media_kind::<media::IndexMedia>(
                    fetched.content_type.as_deref(),
                    document.media_type.clone(),
                )?;
                Ok(FetchedManifest::Index {
                    document,
                    digest: fetched.digest,
                    media_type,
                })
            }
            ManifestDocument::Manifest(document) => {
                let kind = resolve_media_kind::<media::ManifestMedia>(
                    fetched.content_type.as_deref(),
                    document.media_kind.clone(),
                )?;
                Ok(FetchedManifest::Manifest(FetchedImageManifest {
                    document,
                    digest: fetched.digest,
                    kind,
                }))
            }
        }
    }

    #[tracing::instrument(skip(self, desc), fields(url = %url, digest = %desc.digest), err)]
    async fn fetch_config(
        &mut self,
        url: OutboundUrl<'_>,
        desc: &descriptor::Config,
    ) -> Result<ImageConfig, OciError> {
        let fetched = self
            .fetch_bytes(url, desc.media_kind.as_ref(), Some(desc.expectation()))
            .await?;
        serde_json::from_slice(&fetched.body).map_err(|source| OciError::Json {
            url: url.to_string(),
            source,
        })
    }

    #[tracing::instrument(skip(self, desc), fields(url = %url, digest = %desc.digest, expected_size = desc.size), err)]
    async fn fetch_layer(
        &mut self,
        url: OutboundUrl<'_>,
        desc: &descriptor::Layer,
    ) -> Result<api_schema::Layer, OciError> {
        let response = self.get(url, desc.media_kind.as_ref()).await?;
        let mut stream = response.bytes_stream();
        let mut hasher = Sha256::new();
        let mut downloaded_size = 0_u64;

        while let Some(chunk) = stream
            .try_next()
            .await
            .map_err(|source| OciError::Request {
                url: url.to_string(),
                source,
            })?
        {
            downloaded_size = downloaded_size
                .checked_add(u64::try_from(chunk.len()).map_err(|_| {
                    OciError::InvalidDocument("layer chunk length does not fit in u64".to_owned())
                })?)
                .ok_or_else(|| {
                    OciError::InvalidDocument("downloaded layer size overflowed u64".to_owned())
                })?;
            hasher.update(&chunk);
        }

        let downloaded_digest = digest::format(&hasher.finalize())
            .parse()
            .expect("a computed SHA-256 digest should parse");
        api_schema::Layer::new(LayerResponseInput {
            url: url.as_ref(),
            descriptor: desc,
            downloaded_size,
            downloaded_digest,
        })
    }

    #[tracing::instrument(skip(self, expected), fields(url = %url), err)]
    async fn fetch_bytes(
        &mut self,
        url: OutboundUrl<'_>,
        accept: &str,
        expected: Option<descriptor::BlobExpectation<'_>>,
    ) -> Result<FetchedBytes, OciError> {
        let response = self.get(url, accept).await?;
        let content_type = response_content_type(response.headers());
        let header_digest = response
            .headers()
            .get("docker-content-digest")
            .map(|value| {
                value
                    .to_str()
                    .map_err(|source| OciError::Header {
                        url: url.to_string(),
                        source,
                    })?
                    .parse()
                    .map_err(OciError::from)
            })
            .transpose()?;
        let body = response
            .bytes()
            .await
            .map_err(|source| OciError::Request {
                url: url.to_string(),
                source,
            })?
            .to_vec();
        let size = u64::try_from(body.len()).map_err(|_| {
            OciError::InvalidDocument("response body length does not fit in u64".to_owned())
        })?;
        let digest = sha256_digest(&body);

        FetchedBytes::new(FetchedBytesInput {
            url: url.as_ref(),
            body,
            digest,
            size,
            content_type,
            expected,
            header_digest,
        })
    }

    #[tracing::instrument(skip(self), fields(url = %url), err)]
    async fn get(&mut self, url: OutboundUrl<'_>, accept: &str) -> Result<Response, OciError> {
        let response = self.send(url, accept).await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return ensure_success(url.as_ref(), response);
        }

        let challenge = bearer_challenge(response.headers(), url.as_ref())?;
        self.bearer_token = Some(self.fetch_token(&challenge).await?);
        ensure_success(url.as_ref(), self.send(url, accept).await?)
    }

    #[tracing::instrument(skip(self), fields(url = %url), err)]
    async fn send(&self, url: OutboundUrl<'_>, accept: &str) -> Result<Response, OciError> {
        let mut request = self.client.get(url.as_ref().clone()).header(ACCEPT, accept);
        if let Some(token) = &self.bearer_token {
            request = request.bearer_auth(token);
        }
        request.send().await.map_err(|source| OciError::Request {
            url: url.to_string(),
            source,
        })
    }

    #[tracing::instrument(skip(self, challenge), fields(realm = %challenge.realm), err)]
    async fn fetch_token(&self, challenge: &BearerChallenge) -> Result<String, OciError> {
        let mut token_url = challenge.realm.clone();
        {
            let mut query = token_url.query_pairs_mut();
            if let Some(service) = &challenge.service {
                query.append_pair("service", service);
            }
            if let Some(scope) = &challenge.scope {
                query.append_pair("scope", scope);
            }
        }
        let token_url = self.transport.parse(&token_url)?;

        let response = self
            .client
            .get(token_url.as_ref().clone())
            .send()
            .await
            .map_err(|source| OciError::Request {
                url: token_url.to_string(),
                source,
            })?;
        let response = ensure_success(token_url.as_ref(), response)?;
        let token = response
            .json::<TokenResponse>()
            .await
            .map_err(|source| OciError::Request {
                url: token_url.to_string(),
                source,
            })?;
        token
            .token
            .or(token.access_token)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                OciError::Authentication("token service returned no bearer token".to_owned())
            })
    }
}

#[derive(Debug)]
struct FetchedBytesInput<'a> {
    url: &'a Url,
    body: Vec<u8>,
    digest: Sha256Digest,
    size: u64,
    content_type: Option<String>,
    expected: Option<descriptor::BlobExpectation<'a>>,
    header_digest: Option<Sha256Digest>,
}

#[derive(Debug)]
struct FetchedBytes {
    body: Vec<u8>,
    digest: Sha256Digest,
    content_type: Option<String>,
}

impl FetchedBytes {
    fn new(input: FetchedBytesInput<'_>) -> Result<Self, OciError> {
        if let Some(expected) = input.expected {
            if expected.size != input.size {
                return Err(OciError::SizeMismatch {
                    url: input.url.to_string(),
                    expected: expected.size,
                    actual: input.size,
                });
            }
            if expected.digest != &input.digest {
                return Err(OciError::DigestMismatch {
                    url: input.url.to_string(),
                    expected: expected.digest.to_string(),
                    actual: input.digest.to_string(),
                });
            }
        }
        if input
            .header_digest
            .as_ref()
            .is_some_and(|header| header != &input.digest)
        {
            let header = input
                .header_digest
                .expect("a mismatched response digest should be present");
            return Err(OciError::DigestMismatch {
                url: input.url.to_string(),
                expected: header.to_string(),
                actual: input.digest.to_string(),
            });
        }

        Ok(Self {
            body: input.body,
            digest: input.digest,
            content_type: input.content_type,
        })
    }
}

#[derive(Debug)]
struct LayerResponseInput<'a> {
    url: &'a Url,
    descriptor: &'a descriptor::Layer,
    downloaded_size: u64,
    downloaded_digest: Sha256Digest,
}

#[derive(Debug)]
enum FetchedManifest {
    Index {
        document: ImageIndex,
        digest: Sha256Digest,
        media_type: media::IndexMediaKind,
    },
    Manifest(FetchedImageManifest),
}

#[derive(Debug)]
struct FetchedImageManifest {
    document: oci_schema::ImageManifest,
    digest: Sha256Digest,
    kind: media::ManifestMediaKind,
}

#[derive(Debug)]
struct BearerChallenge {
    realm: Url,
    service: Option<String>,
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

#[tracing::instrument(skip(headers), fields(url = %url), err)]
fn bearer_challenge(headers: &HeaderMap, url: &Url) -> Result<BearerChallenge, OciError> {
    let value = headers
        .get(WWW_AUTHENTICATE)
        .ok_or_else(|| {
            OciError::Authentication("registry returned 401 without a challenge".to_owned())
        })?
        .to_str()
        .map_err(|source| OciError::Header {
            url: url.to_string(),
            source,
        })?;
    let (scheme, parameters) = value.split_once(' ').ok_or_else(|| {
        OciError::Authentication(
            "registry returned a malformed authentication challenge".to_owned(),
        )
    })?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(OciError::Authentication(format!(
            "unsupported registry authentication scheme {scheme:?}"
        )));
    }

    let parameters = parse_auth_parameters(parameters)?;
    let realm = parameters
        .get("realm")
        .ok_or_else(|| OciError::Authentication("bearer challenge has no realm".to_owned()))?;
    let realm = Url::parse(realm)
        .map_err(|error| OciError::Authentication(format!("invalid bearer realm: {error}")))?;
    Ok(BearerChallenge {
        realm,
        service: parameters.get("service").cloned(),
        scope: parameters.get("scope").cloned(),
    })
}

#[tracing::instrument(err)]
fn parse_auth_parameters(value: &str) -> Result<BTreeMap<String, String>, OciError> {
    let mut parameters = BTreeMap::new();
    for parameter in value.split(',') {
        let (key, value) = parameter.trim().split_once('=').ok_or_else(|| {
            OciError::Authentication("malformed bearer challenge parameter".to_owned())
        })?;
        let value = value.trim();
        if value.len() < 2 || !value.starts_with('"') || !value.ends_with('"') {
            return Err(OciError::Authentication(
                "bearer challenge values must be quoted".to_owned(),
            ));
        }
        parameters.insert(
            key.to_ascii_lowercase(),
            value[1..value.len() - 1].to_owned(),
        );
    }
    Ok(parameters)
}

#[tracing::instrument(skip(headers))]
fn response_content_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[tracing::instrument(skip(response, document), err)]
fn resolve_media_kind<K: media::MediaTypeKind + PartialEq>(
    response: Option<&str>,
    document: Option<media::MediaKind<K>>,
) -> Result<media::MediaKind<K>, OciError> {
    let response = response
        .map(str::parse::<media::MediaKind<K>>)
        .transpose()
        .map_err(OciError::from)?;
    match (response, document) {
        (Some(response), Some(document)) if response != document => {
            Err(OciError::InvalidDocument(format!(
                "response media type {:?} does not match document media type {:?}",
                response.as_ref(),
                document.as_ref()
            )))
        }
        (Some(response), _) => Ok(response),
        (None, Some(document)) => Ok(document),
        (None, None) => Err(OciError::InvalidDocument(
            "manifest has no media type".to_owned(),
        )),
    }
}

#[tracing::instrument(err)]
fn ensure_success(url: &Url, response: Response) -> Result<Response, OciError> {
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(OciError::UpstreamStatus {
            url: url.to_string(),
            status: response.status(),
        })
    }
}

mod media {
    use std::{
        fmt::{self, Display},
        marker::PhantomData,
        str::FromStr,
    };

    use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

    use super::{OciError, ParseValueError};

    pub type IndexMediaKind = MediaKind<IndexMedia>;
    pub type ManifestMediaKind = MediaKind<ManifestMedia>;
    pub type ConfigMediaKind = MediaKind<ConfigMedia>;
    pub type LayerMediaKind = MediaKind<LayerMedia>;

    pub const ACCEPT: &str = concat!(
        "application/vnd.oci.image.index.v1+json, ",
        "application/vnd.docker.distribution.manifest.list.v2+json, ",
        "application/vnd.oci.image.manifest.v1+json, ",
        "application/vnd.docker.distribution.manifest.v2+json"
    );

    pub trait MediaTypeKind {
        const NAME: &'static str;
        const SUPPORTED: &'static [&'static str];
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct IndexMedia;

    impl MediaTypeKind for IndexMedia {
        const NAME: &'static str = "image index media type";
        const SUPPORTED: &'static [&'static str] = &[
            "application/vnd.oci.image.index.v1+json",
            "application/vnd.docker.distribution.manifest.list.v2+json",
        ];
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ManifestMedia;

    impl MediaTypeKind for ManifestMedia {
        const NAME: &'static str = "image manifest media type";
        const SUPPORTED: &'static [&'static str] = &[
            "application/vnd.oci.image.manifest.v1+json",
            "application/vnd.docker.distribution.manifest.v2+json",
        ];
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ConfigMedia;

    impl MediaTypeKind for ConfigMedia {
        const NAME: &'static str = "image config media type";
        const SUPPORTED: &'static [&'static str] = &[
            "application/vnd.oci.image.config.v1+json",
            "application/vnd.docker.container.image.v1+json",
        ];
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct LayerMedia;

    impl MediaTypeKind for LayerMedia {
        const NAME: &'static str = "image layer media type";
        const SUPPORTED: &'static [&'static str] = &[
            "application/vnd.oci.image.layer.v1.tar",
            "application/vnd.oci.image.layer.v1.tar+gzip",
            "application/vnd.oci.image.layer.v1.tar+zstd",
            "application/vnd.docker.image.rootfs.diff.tar.gzip",
        ];
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct MediaKind<K> {
        value: String,
        kind: PhantomData<fn() -> K>,
    }

    impl<K: MediaTypeKind> FromStr for MediaKind<K> {
        type Err = ParseValueError;

        fn from_str(value: &str) -> Result<Self, Self::Err> {
            K::SUPPORTED
                .contains(&value)
                .then(|| Self {
                    value: value.to_owned(),
                    kind: PhantomData,
                })
                .ok_or_else(|| ParseValueError::new(K::NAME, value))
        }
    }

    impl<K> AsRef<str> for MediaKind<K> {
        fn as_ref(&self) -> &str {
            &self.value
        }
    }

    impl<K> Display for MediaKind<K> {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.value.fmt(formatter)
        }
    }

    impl<K> Serialize for MediaKind<K> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            self.value.serialize(serializer)
        }
    }

    impl<'de, K: MediaTypeKind> Deserialize<'de> for MediaKind<K> {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            String::deserialize(deserializer)?
                .parse()
                .map_err(de::Error::custom)
        }
    }
}

mod api_schema {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};

    use super::{
        Arch, LayerResponseInput, OS, OciError, RegistryLoc, Sha256Digest,
        descriptor::{History, Rootfs},
        media::{ConfigMediaKind, IndexMediaKind, ManifestMediaKind},
    };

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct RunInput {
        pub url: RegistryLoc,
    }

    #[derive(Debug, Serialize)]
    pub struct Run {
        pub url: String,
        pub platform: Platform,
        pub index: Option<Document>,
        pub manifest: Manifest,
        pub image: Image,
        pub rootfs: Rootfs,
        pub history: Vec<History>,
        pub layers: Vec<Layer>,
    }

    #[derive(Debug, Serialize)]
    pub struct Platform {
        pub os: OS,
        pub architecture: Arch,
        pub variant: Option<String>,
        pub os_version: Option<String>,
        pub os_features: Vec<String>,
    }

    #[derive(Debug, Serialize)]
    pub struct Document {
        pub schema_version: u32,
        pub media_kind: IndexMediaKind,
        pub digest: Sha256Digest,
        pub annotations: BTreeMap<String, String>,
    }

    #[derive(Debug, Serialize)]
    pub struct Manifest {
        pub schema_version: u32,
        pub media_kind: ManifestMediaKind,
        pub digest: Sha256Digest,
        pub annotations: BTreeMap<String, String>,
        pub config: Descriptor,
    }

    #[derive(Debug, Serialize)]
    pub struct Descriptor {
        pub media_kind: ConfigMediaKind,
        pub digest: Sha256Digest,
        pub size: u64,
    }

    #[derive(Debug, Serialize)]
    pub struct Image {
        pub created: Option<String>,
        pub author: Option<String>,
        pub user: Option<String>,
        pub exposed_ports: Vec<String>,
        pub env: Vec<String>,
        pub entrypoint: Vec<String>,
        pub cmd: Vec<String>,
        pub volumes: Vec<String>,
        pub working_dir: Option<String>,
        pub labels: BTreeMap<String, String>,
        pub stop_signal: Option<String>,
        pub args_escaped: Option<bool>,
    }

    #[derive(Debug, Serialize)]
    pub struct Layer {
        url: String,
        media_kind: super::media::LayerMediaKind,
        digest: Sha256Digest,
        declared_size: u64,
        downloaded_size: u64,
    }

    impl Layer {
        pub fn new(input: LayerResponseInput<'_>) -> Result<Self, OciError> {
            if input.descriptor.size != input.downloaded_size {
                return Err(OciError::SizeMismatch {
                    url: input.url.to_string(),
                    expected: input.descriptor.size,
                    actual: input.downloaded_size,
                });
            }
            if input.descriptor.digest != input.downloaded_digest {
                return Err(OciError::DigestMismatch {
                    url: input.url.to_string(),
                    expected: input.descriptor.digest.to_string(),
                    actual: input.downloaded_digest.to_string(),
                });
            }

            Ok(Self {
                url: input.url.to_string(),
                media_kind: input.descriptor.media_kind.clone(),
                digest: input.descriptor.digest.clone(),
                declared_size: input.descriptor.size,
                downloaded_size: input.downloaded_size,
            })
        }
    }
}

mod oci_schema {
    use std::collections::BTreeMap;

    use serde::Deserialize;

    use super::{
        Arch, OS,
        descriptor::{Config, Layer},
        media::ManifestMediaKind,
    };

    #[derive(Debug, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct PlatformDesc {
        #[serde(rename = "architecture")]
        pub arch: Arch,
        pub os: OS,
        pub variant: Option<String>,
        #[serde(rename = "os.version", default)]
        pub os_version: Option<String>,
        #[serde(rename = "os.features", default)]
        pub os_features: Vec<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ImageManifest {
        pub schema_version: u32,
        pub media_kind: Option<ManifestMediaKind>,
        pub config: Config,
        pub layers: Vec<Layer>,
        #[serde(default)]
        pub annotations: BTreeMap<String, String>,
    }

    #[derive(Debug, Default, Deserialize)]
    pub struct RuntimeConfig {
        #[serde(rename = "User")]
        pub user: Option<String>,
        #[serde(rename = "ExposedPorts", default)]
        pub exposed_ports: BTreeMap<String, serde_json::Value>,
        #[serde(rename = "Env", default)]
        pub env: Vec<String>,
        #[serde(rename = "Entrypoint", default)]
        pub entrypoint: Vec<String>,
        #[serde(rename = "Cmd", default)]
        pub cmd: Vec<String>,
        #[serde(rename = "Volumes", default)]
        pub volumes: BTreeMap<String, serde_json::Value>,
        #[serde(rename = "WorkingDir")]
        pub working_dir: Option<String>,
        #[serde(rename = "Labels", default)]
        pub labels: BTreeMap<String, String>,
        #[serde(rename = "StopSignal")]
        pub stop_signal: Option<String>,
        #[serde(rename = "ArgsEscaped")]
        pub args_escaped: Option<bool>,
    }

    #[derive(Debug, Deserialize, Serialize)]
    #[serde(tag = "type")]
    pub enum Rootfs {
        #[serde(rename = "layers")]
        Layers { diff_ids: Vec<Sha256Digest> },
    }

    impl Rootfs {
        pub fn new(doc: RootfsDoc, layers: &[Layer]) -> Result<Self, ParseImageConfigError> {
            let RootfsDoc::Layers { diff_ids } = doc;
            if diff_ids.len() != layers.len() {
                return Err(ParseImageConfigError::Rootfs {
                    diff_id_count: diff_ids.len(),
                    layer_count: layers.len(),
                });
            }
            Ok(Self::Layers { diff_ids })
        }
    }

    #[derive(Debug, Deserialize, Serialize)]
    pub struct History {
        pub created: Option<String>,
        pub created_by: Option<String>,
        pub author: Option<String>,
        pub comment: Option<String>,
        #[serde(default)]
        pub empty_layer: bool,
    }

    #[derive(Debug, Deserialize)]
    #[serde(untagged)]
    enum ManifestDocument {
        Index(ImageIndex),
        Manifest(oci_schema::ImageManifest),
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ImageIndex {
        schema_version: u32,
        media_type: Option<media::IndexMediaKind>,
        manifests: Vec<descriptor::Manifest>,
        #[serde(default)]
        annotations: BTreeMap<String, String>,
    }
}

mod descriptor {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};

    use super::{
        Sha256Digest,
        media::{ConfigMedia, LayerMedia, ManifestMedia, MediaKind, MediaTypeKind},
        oci_schema::PlatformDesc,
    };

    #[derive(Debug, Clone, Deserialize)]
    #[serde(rename_all = "camelCase", bound(deserialize = ""))]
    pub struct Descriptor<K: MediaTypeKind> {
        pub media_kind: MediaKind<K>,
        pub digest: Sha256Digest,
        pub size: u64,
        #[serde(default)]
        pub platform: Option<PlatformDesc>,
    }

    impl<K: MediaTypeKind> Descriptor<K> {
        pub fn expectation(&self) -> BlobExpectation<'_> {
            BlobExpectation {
                digest: &self.digest,
                size: self.size,
            }
        }
    }

    pub type Manifest = Descriptor<ManifestMedia>;
    pub type Config = Descriptor<ConfigMedia>;
    pub type Layer = Descriptor<LayerMedia>;

    #[derive(Debug, Clone, Copy)]
    pub struct BlobExpectation<'a> {
        pub digest: &'a Sha256Digest,
        pub size: u64,
    }
}

mod image {
    use super::*;

    type Result<T, E = ParseImageConfigError> = std::result::Result<T, E>;

    #[derive(Debug)]
    struct Config {
        created: Option<String>,
        author: Option<String>,
        platform: Platform,
        config: oci_schema::RuntimeConfig,
        rootfs: oci_schema::Rootfs,
        history: Vec<oci_schema::History>,
    }

    impl ImageConfig {
        fn new(
            doc: ImageConfigDoc,
            host: &HostPlatform,
            layers: &[descriptor::Layer],
        ) -> Result<Self> {
            Ok(Self {
                created: doc.created,
                author: doc.author,
                platform: ImagePlatform::new(doc.platform, host)?,
                config: doc.config,
                rootfs: descriptor::Rootfs::new(doc.rootfs, layers)?,
                history: doc.history,
            })
        }

        #[tracing::instrument(skip(self, selected))]
        fn platform(&self, selected: Option<oci_schema::PlatformDesc>) -> api_schema::Platform {
            api_schema::Platform {
                os: self.platform.os.clone(),
                architecture: self.platform.architecture.clone(),
                variant: selected
                    .as_ref()
                    .and_then(|platform| platform.variant.clone())
                    .or_else(|| self.platform.variant.clone()),
                os_version: selected
                    .as_ref()
                    .and_then(|platform| platform.os_version.clone())
                    .or_else(|| self.platform.os_version.clone()),
                os_features: selected
                    .filter(|platform| !platform.os_features.is_empty())
                    .map_or_else(
                        || self.platform.os_features.clone(),
                        |platform| platform.os_features,
                    ),
            }
        }

        #[tracing::instrument(skip(self))]
        fn response(&self) -> api_schema::Image {
            let config = &self.config;
            api_schema::Image {
                created: self.created.clone(),
                author: self.author.clone(),
                user: config.user.clone(),
                exposed_ports: config.exposed_ports.keys().cloned().collect(),
                env: config.env.clone(),
                entrypoint: config.entrypoint.clone(),
                cmd: config.cmd.clone(),
                volumes: config.volumes.keys().cloned().collect(),
                working_dir: config.working_dir.clone(),
                labels: config.labels.clone(),
                stop_signal: config.stop_signal.clone(),
                args_escaped: config.args_escaped,
            }
        }
    }

    #[derive(Debug)]
    struct ImagePlatform {
        architecture: Arch,
        os: OS,
        variant: Option<String>,
        os_version: Option<String>,
        os_features: Vec<String>,
    }

    impl ImagePlatform {
        fn new(platform: oci_schema::PlatformDesc, host: &HostPlatform) -> Result<Self> {
            if platform.os != host.os || platform.arch != host.architecture {
                return Err(oci_schema::ParseImageConfigError::Platform {
                    image_os: platform.os,
                    image_architecture: platform.arch,
                    host_os: host.os.clone(),
                    host_architecture: host.architecture.clone(),
                });
            }

            Ok(Self {
                architecture: platform.arch,
                os: platform.os,
                variant: platform.variant,
                os_version: platform.os_version,
                os_features: platform.os_features,
            })
        }
    }

    #[derive(Debug, thiserror::Error)]
    pub enum ParseImageConfigError {
        #[error(
            "image platform {image_os}/{image_architecture} does not match host {host_os}/{host_architecture}"
        )]
        Platform {
            image_os: OS,
            image_architecture: Arch,
            host_os: OS,
            host_architecture: Arch,
        },
        #[error("rootfs has {diff_id_count} diff IDs but manifest has {layer_count} layers")]
        Rootfs {
            diff_id_count: usize,
            layer_count: usize,
        },
    }
}

mod registry {
    use super::*;

    #[derive(Debug)]
    pub struct RegistryLoc {
        manifest: Url,
    }

    impl FromStr for RegistryLoc {
        type Err = OciError;

        fn from_str(value: &str) -> Result<Self, Self::Err> {
            let url = Url::parse(value).map_err(|error| {
                OciError::InvalidInput(format!("invalid OCI manifest URL: {error}"))
            })?;
            if !url.username().is_empty() || url.password().is_some() {
                return Err(OciError::InvalidInput(
                    "OCI manifest URL must not contain credentials".to_owned(),
                ));
            }

            let segments = url
                .path_segments()
                .ok_or_else(|| {
                    OciError::InvalidInput("OCI manifest URL cannot be a base URL".to_owned())
                })?
                .collect::<Vec<_>>();
            if segments.len() < 4
                || segments.first() != Some(&"v2")
                || segments.get(segments.len() - 2) != Some(&"manifests")
                || segments[1..segments.len() - 2]
                    .iter()
                    .any(|segment| segment.is_empty())
                || segments.last().is_some_and(|segment| segment.is_empty())
            {
                return Err(OciError::InvalidInput(
                    "OCI URL must match /v2/<repository>/manifests/<tag-or-digest>".to_owned(),
                ));
            }
            Ok(Self { manifest: url })
        }
    }

    impl<'de> Deserialize<'de> for RegistryLoc {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            String::deserialize(deserializer)?
                .parse()
                .map_err(de::Error::custom)
        }
    }

    impl Display for RegistryLoc {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.manifest.fmt(formatter)
        }
    }

    impl RegistryLoc {
        #[tracing::instrument(err)]
        fn manifest_url(&self, digest: &Sha256Digest) -> Result<Url, OciError> {
            self.descriptor_url("manifests", digest)
        }

        #[tracing::instrument(err)]
        fn blob_url(&self, digest: &Sha256Digest) -> Result<Url, OciError> {
            self.descriptor_url("blobs", digest)
        }

        #[tracing::instrument(err)]
        fn descriptor_url(&self, kind: &str, digest: &Sha256Digest) -> Result<Url, OciError> {
            let mut url = self.manifest.clone();
            {
                let mut segments = url.path_segments_mut().map_err(|()| {
                    OciError::InvalidInput("OCI manifest URL cannot be a base URL".to_owned())
                })?;
                segments.pop();
                segments.pop();
                segments.push(kind);
                segments.push(digest.as_ref());
            }
            url.set_query(None);
            url.set_fragment(None);
            Ok(url)
        }
    }
}

mod digest {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub struct Sha256Digest(String);

    impl From<&[u8]> for Sha256Digest {
        #[tracing::instrument(skip(bytes), fields(byte_count = bytes.len()))]
        fn from(bytes: &[u8]) -> Sha256Digest {
            Sha256Digest(format(&Sha256::digest(bytes)))
        }
    }

    #[tracing::instrument(skip(bytes))]
    fn format(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";

        let mut digest = String::with_capacity("sha256:".len() + bytes.len() * 2);
        digest.push_str("sha256:");
        for byte in bytes {
            digest.push(char::from(HEX[usize::from(byte >> 4)]));
            digest.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        digest
    }

    impl FromStr for Sha256Digest {
        type Err = ParseValueError;

        fn from_str(value: &str) -> Result<Self, Self::Err> {
            let hash = value
                .strip_prefix("sha256:")
                .filter(|hash| {
                    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                .ok_or_else(|| ParseValueError::new("SHA-256 digest", value))?;
            Ok(Self(format!("sha256:{hash}")))
        }
    }

    impl AsRef<str> for Sha256Digest {
        fn as_ref(&self) -> &str {
            &self.0
        }
    }

    impl Display for Sha256Digest {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fmt(formatter)
        }
    }

    impl Serialize for Sha256Digest {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            self.0.serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for Sha256Digest {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            String::deserialize(deserializer)?
                .parse()
                .map_err(de::Error::custom)
        }
    }
}

mod platform {
    use super::*;

    #[derive(Debug, Clone)]
    struct HostPlatform {
        os: OS,
        architecture: Arch,
    }

    impl HostPlatform {
        #[tracing::instrument(err)]
        fn current() -> Result<Self, OciError> {
            let os = match std::env::consts::OS {
                "macos" => "darwin",
                os => os,
            };
            let architecture = match std::env::consts::ARCH {
                "x86_64" => "amd64",
                "aarch64" => "arm64",
                architecture => {
                    return Err(OciError::InvalidInput(format!(
                        "unsupported host architecture {architecture:?}"
                    )));
                }
            };
            Ok(Self {
                os: os
                    .parse()
                    .map_err(|error: ParseValueError| OciError::InvalidInput(error.to_string()))?,
                architecture: architecture
                    .parse()
                    .map_err(|error: ParseValueError| OciError::InvalidInput(error.to_string()))?,
            })
        }

        #[tracing::instrument(skip(manifests), fields(os = %self.os, architecture = %self.architecture), err)]
        fn select_platform(
            &self,
            manifests: &[descriptor::Manifest],
        ) -> Result<descriptor::Manifest, OciError> {
            manifests
                .iter()
                .find(|descriptor| {
                    descriptor.platform.as_ref().is_some_and(|platform| {
                        platform.os == self.os && platform.arch == self.architecture
                    })
                })
                .cloned()
                .ok_or_else(|| {
                    OciError::InvalidInput(format!(
                        "image index has no {}/{} manifest",
                        self.os, self.architecture
                    ))
                })
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
    #[serde(transparent)]
    pub struct OS(String);

    impl FromStr for OS {
        type Err = ParseValueError;

        fn from_str(value: &str) -> Result<Self, Self::Err> {
            (!value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()))
            .then(|| Self(value.to_owned()))
            .ok_or_else(|| ParseValueError::new("operating system", value))
        }
    }

    impl AsRef<str> for OS {
        fn as_ref(&self) -> &str {
            &self.0
        }
    }

    impl Display for OS {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fmt(formatter)
        }
    }

    impl<'de> Deserialize<'de> for OS {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            String::deserialize(deserializer)?
                .parse()
                .map_err(de::Error::custom)
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
    #[serde(transparent)]
    pub struct Arch(String);

    impl FromStr for Arch {
        type Err = ParseValueError;

        fn from_str(value: &str) -> Result<Self, Self::Err> {
            (!value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()))
            .then(|| Self(value.to_owned()))
            .ok_or_else(|| ParseValueError::new("architecture", value))
        }
    }

    impl AsRef<str> for Arch {
        fn as_ref(&self) -> &str {
            &self.0
        }
    }

    impl Display for Arch {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fmt(formatter)
        }
    }

    impl<'de> Deserialize<'de> for Arch {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            String::deserialize(deserializer)?
                .parse()
                .map_err(de::Error::custom)
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OciError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("registry request to {url} failed: {source}")]
    Request {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("registry request to {url} returned {status}")]
    UpstreamStatus { url: String, status: StatusCode },
    #[error("registry response from {url} had an invalid header: {source}")]
    Header {
        url: String,
        #[source]
        source: reqwest::header::ToStrError,
    },
    #[error("registry JSON from {url} was invalid: {source}")]
    Json {
        url: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("registry authentication failed: {0}")]
    Authentication(String),
    #[error("invalid OCI document: {0}")]
    InvalidDocument(String),
    #[error("digest mismatch for {url}: expected {expected}, got {actual}")]
    DigestMismatch {
        url: String,
        expected: String,
        actual: String,
    },
    #[error("size mismatch for {url}: expected {expected} bytes, got {actual}")]
    SizeMismatch {
        url: String,
        expected: u64,
        actual: u64,
    },
}

impl From<ParseValueError> for OciError {
    fn from(error: ParseValueError) -> Self {
        Self::InvalidDocument(error.to_string())
    }
}

impl From<oci_schema::ParseImageConfigError> for OciError {
    fn from(error: oci_schema::ParseImageConfigError) -> Self {
        let message = error.to_string();
        match error {
            oci_schema::ParseImageConfigError::Platform { .. } => Self::InvalidInput(message),
            oci_schema::ParseImageConfigError::Rootfs { .. } => Self::InvalidDocument(message),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind}: {value:?}")]
pub struct ParseValueError {
    kind: &'static str,
    value: String,
}

impl ParseValueError {
    fn new(kind: &'static str, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Router,
        body::Body,
        extract::State,
        http::{
            Request, Uri,
            header::{AUTHORIZATION, CONTENT_TYPE},
        },
        response::{IntoResponse, Response},
        routing::{get, post},
    };
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, task::JoinHandle};
    use tower::ServiceExt;

    use super::*;
    use super::{
        descriptor::{Descriptor, Layer as LayerDesc, ParseImageConfigError},
        oci_schema::ImageConfigDoc,
    };

    const TOKEN: &str = "fixture-token";

    #[derive(Debug, Clone, Copy)]
    enum InitialDocument {
        Index,
        Manifest,
    }

    #[derive(Debug, Clone)]
    struct ServedDocument {
        body: Vec<u8>,
        media_type: &'static str,
        digest: Sha256Digest,
    }

    impl ServedDocument {
        fn from_json(value: Value, media_type: &'static str) -> Self {
            let body = serde_json::to_vec(&value).expect("fixture JSON should serialize");
            let digest = sha256_digest(&body);
            Self {
                body,
                media_type,
                digest,
            }
        }
    }

    #[derive(Debug)]
    struct RegistryDocuments {
        initial: ServedDocument,
        manifest: ServedDocument,
        config: ServedDocument,
        layers: Vec<(Sha256Digest, &'static str, Vec<u8>)>,
    }

    impl RegistryDocuments {
        fn new(
            platform: &HostPlatform,
            initial: InitialDocument,
            corrupt_layer_digest: bool,
        ) -> Self {
            let first_layer = b"first compressed filesystem layer".to_vec();
            let second_layer = b"second compressed filesystem layer".to_vec();
            let first_digest = if corrupt_layer_digest {
                format!("sha256:{}", "0".repeat(64))
                    .parse()
                    .expect("fixture digest should be valid")
            } else {
                sha256_digest(&first_layer)
            };
            let second_digest = sha256_digest(&second_layer);

            let config = ServedDocument::from_json(
                json!({
                    "created": "2026-08-02T12:00:00Z",
                    "author": "fixture@example.com",
                    "architecture": platform.architecture,
                    "os": platform.os,
                    "variant": "v8",
                    "os.version": "fixture-os-version",
                    "os.features": ["fixture-feature"],
                    "config": {
                        "User": "1000:1000",
                        "ExposedPorts": {
                            "443/tcp": {},
                            "80/tcp": {}
                        },
                        "Env": ["PATH=/usr/bin", "MODE=test"],
                        "Entrypoint": ["/bin/fixture"],
                        "Cmd": ["serve", "--foreground"],
                        "Volumes": {
                            "/cache": {},
                            "/data": {}
                        },
                        "WorkingDir": "/work",
                        "Labels": {
                            "org.opencontainers.image.title": "fixture"
                        },
                        "StopSignal": "SIGTERM",
                        "ArgsEscaped": false
                    },
                    "rootfs": {
                        "type": "layers",
                        "diff_ids": [
                            sha256_digest(b"first uncompressed layer"),
                            sha256_digest(b"second uncompressed layer")
                        ]
                    },
                    "history": [
                        {
                            "created": "2026-08-02T11:59:00Z",
                            "created_by": "ADD rootfs /",
                            "author": "fixture@example.com",
                            "comment": "base",
                            "empty_layer": false
                        },
                        {
                            "created": "2026-08-02T12:00:00Z",
                            "created_by": "CMD serve",
                            "empty_layer": true
                        }
                    ]
                }),
                "application/vnd.oci.image.config.v1+json",
            );

            let manifest = ServedDocument::from_json(
                json!({
                    "schemaVersion": 2,
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "config": {
                        "mediaType": "application/vnd.oci.image.config.v1+json",
                        "digest": config.digest,
                        "size": config.body.len()
                    },
                    "layers": [
                        {
                            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                            "digest": first_digest,
                            "size": first_layer.len()
                        },
                        {
                            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                            "digest": second_digest,
                            "size": second_layer.len()
                        }
                    ],
                    "annotations": {
                        "org.opencontainers.image.ref.name": "latest"
                    }
                }),
                "application/vnd.oci.image.manifest.v1+json",
            );

            let index = ServedDocument::from_json(
                json!({
                    "schemaVersion": 2,
                    "mediaType": "application/vnd.oci.image.index.v1+json",
                    "manifests": [
                        {
                            "mediaType": "application/vnd.oci.image.manifest.v1+json",
                            "digest": manifest.digest,
                            "size": manifest.body.len(),
                            "platform": {
                                "architecture": platform.architecture,
                                "os": platform.os,
                                "variant": "v8",
                                "os.version": "index-os-version",
                                "os.features": ["index-feature"]
                            }
                        }
                    ],
                    "annotations": {
                        "org.opencontainers.image.ref.name": "fixture-index"
                    }
                }),
                "application/vnd.oci.image.index.v1+json",
            );

            let initial = match initial {
                InitialDocument::Index => index,
                InitialDocument::Manifest => manifest.clone(),
            };
            Self {
                initial,
                manifest,
                config,
                layers: vec![
                    (
                        first_digest,
                        "application/vnd.oci.image.layer.v1.tar+gzip",
                        first_layer,
                    ),
                    (
                        second_digest,
                        "application/vnd.oci.image.layer.v1.tar+gzip",
                        second_layer,
                    ),
                ],
            }
        }
    }

    #[derive(Clone)]
    struct RegistryState {
        base_url: String,
        documents: Arc<RegistryDocuments>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    struct RegistryFixture {
        manifest_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        task: JoinHandle<()>,
    }

    impl RegistryFixture {
        async fn start(documents: RegistryDocuments) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("fixture registry should bind");
            let address = listener
                .local_addr()
                .expect("fixture registry should have an address");
            let base_url = format!("http://{address}");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let state = RegistryState {
                base_url: base_url.clone(),
                documents: Arc::new(documents),
                requests: requests.clone(),
            };
            let app = Router::new()
                .route("/token", get(token))
                .route("/v2/{*path}", get(registry))
                .with_state(state);
            let task = tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("fixture registry should serve");
            });
            Self {
                manifest_url: format!("{base_url}/v2/library/fixture/manifests/latest"),
                requests,
                task,
            }
        }

        fn requests(&self) -> Vec<String> {
            self.requests
                .lock()
                .expect("fixture request log should not be poisoned")
                .clone()
        }
    }

    impl Drop for RegistryFixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn token(State(state): State<RegistryState>) -> Json<Value> {
        state
            .requests
            .lock()
            .expect("fixture request log should not be poisoned")
            .push("token".to_owned());
        Json(json!({ "token": TOKEN }))
    }

    async fn registry(
        State(state): State<RegistryState>,
        headers: HeaderMap,
        uri: Uri,
    ) -> Response {
        let authorized = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == format!("Bearer {TOKEN}"));
        state
            .requests
            .lock()
            .expect("fixture request log should not be poisoned")
            .push(format!("{} authorized={authorized}", uri.path()));
        if !authorized {
            let challenge = format!(
                "Bearer realm=\"{}/token\",service=\"fixture-registry\",scope=\"repository:library/fixture:pull\"",
                state.base_url
            );
            return Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header(WWW_AUTHENTICATE, challenge)
                .body(Body::empty())
                .expect("fixture unauthorized response should build");
        }

        let path = uri.path();
        let documents = &state.documents;
        if path == "/v2/library/fixture/manifests/latest" {
            return document_response(&documents.initial);
        }
        if path
            == format!(
                "/v2/library/fixture/manifests/{}",
                documents.manifest.digest
            )
        {
            return document_response(&documents.manifest);
        }
        if path == format!("/v2/library/fixture/blobs/{}", documents.config.digest) {
            return document_response(&documents.config);
        }
        for (digest, media_type, body) in &documents.layers {
            if path == format!("/v2/library/fixture/blobs/{digest}") {
                return bytes_response(body, media_type, digest);
            }
        }
        StatusCode::NOT_FOUND.into_response()
    }

    fn document_response(document: &ServedDocument) -> Response {
        bytes_response(&document.body, document.media_type, &document.digest)
    }

    fn bytes_response(body: &[u8], media_type: &str, digest: &Sha256Digest) -> Response {
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, media_type)
            .header("docker-content-digest", digest.as_ref())
            .body(Body::from(body.to_vec()))
            .expect("fixture document response should build")
    }

    async fn request_run(fetcher: OciFetcher, url: &str) -> (StatusCode, Value) {
        let app = Router::new()
            .route("/run", post(run))
            .layer(Extension(fetcher));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/run")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({ "url": url }))
                            .expect("request JSON should serialize"),
                    ))
                    .expect("request should build"),
            )
            .await
            .expect("run router should respond");
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("response body should collect")
            .to_bytes();
        let body = serde_json::from_slice(&body).expect("response should contain JSON");
        (status, body)
    }

    fn layer_descriptors(count: usize) -> Vec<LayerDesc> {
        (0..count)
            .map(|index| Descriptor {
                media_kind: "application/vnd.oci.image.layer.v1.tar"
                    .parse()
                    .expect("fixture media type should parse"),
                digest: sha256_digest(index.to_string().as_bytes()),
                size: 0,
                platform: None,
            })
            .collect()
    }

    fn configuration_document(platform: &HostPlatform, diff_id_count: usize) -> ImageConfigDoc {
        let diff_ids = (0..diff_id_count)
            .map(|index| sha256_digest(index.to_string().as_bytes()))
            .collect::<Vec<_>>();
        serde_json::from_value(json!({
            "architecture": platform.architecture,
            "os": platform.os,
            "rootfs": {
                "type": "layers",
                "diff_ids": diff_ids,
            },
        }))
        .expect("fixture configuration should parse")
    }

    #[test]
    fn platform_fields_parse_during_deserialization() {
        let platform = HostPlatform::current().expect("test host should be supported");
        let error = serde_json::from_value::<ImageConfigDoc>(json!({
            "architecture": "AMD64",
            "os": platform.os,
            "rootfs": {
                "type": "layers",
                "diff_ids": [],
            },
        }))
        .expect_err("an invalid architecture must not produce a document");

        assert!(error.to_string().contains("invalid architecture"));
    }

    #[test]
    fn image_configuration_parser_rejects_a_different_platform() {
        let platform = HostPlatform::current().expect("test host should be supported");
        let mut document = configuration_document(&platform, 1);
        document.platform.arch = if platform.architecture.as_ref() == "amd64" {
            "arm64".parse().expect("fixture architecture should parse")
        } else {
            "amd64".parse().expect("fixture architecture should parse")
        };

        let error = ImageConfig::new(document, &platform, &layer_descriptors(1))
            .expect_err("a different platform must not produce an image configuration");

        assert!(matches!(error, ParseImageConfigError::Platform { .. }));
    }

    #[test]
    fn image_configuration_parser_rejects_a_rootfs_for_different_layers() {
        let platform = HostPlatform::current().expect("test host should be supported");
        let document = configuration_document(&platform, 1);

        let error = ImageConfig::new(document, &platform, &layer_descriptors(2))
            .expect_err("a mismatched rootfs must not produce an image configuration");

        assert!(matches!(
            error,
            ParseImageConfigError::Rootfs {
                diff_id_count: 1,
                layer_count: 2,
            }
        ));
    }

    #[tokio::test]
    async fn run_fetches_anonymous_host_image_and_verifies_every_layer() {
        let platform = HostPlatform::current().expect("test host should be supported");
        let registry = RegistryFixture::start(RegistryDocuments::new(
            &platform,
            InitialDocument::Index,
            false,
        ))
        .await;

        let (status, body) = request_run(
            OciFetcher::for_test(platform.clone()),
            &registry.manifest_url,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["url"], registry.manifest_url);
        assert_eq!(body["platform"]["os"], platform.os.as_ref());
        assert_eq!(
            body["platform"]["architecture"],
            platform.architecture.as_ref()
        );
        assert_eq!(body["platform"]["variant"], "v8");
        assert_eq!(body["platform"]["os_version"], "index-os-version");
        assert_eq!(body["platform"]["os_features"], json!(["index-feature"]));
        assert_eq!(
            body["index"]["media_type"],
            "application/vnd.oci.image.index.v1+json"
        );
        assert_eq!(
            body["manifest"]["media_type"],
            "application/vnd.oci.image.manifest.v1+json"
        );
        assert_eq!(
            body["manifest"]["config"]["media_type"],
            "application/vnd.oci.image.config.v1+json"
        );
        assert_eq!(body["image"]["user"], "1000:1000");
        assert_eq!(body["image"]["entrypoint"], json!(["/bin/fixture"]));
        assert_eq!(body["image"]["cmd"], json!(["serve", "--foreground"]));
        assert_eq!(body["image"]["exposed_ports"], json!(["443/tcp", "80/tcp"]));
        assert_eq!(body["image"]["volumes"], json!(["/cache", "/data"]));
        assert_eq!(body["rootfs"]["type"], "layers");
        assert_eq!(body["rootfs"]["diff_ids"].as_array().map(Vec::len), Some(2));
        assert_eq!(body["history"].as_array().map(Vec::len), Some(2));
        assert_eq!(body["layers"].as_array().map(Vec::len), Some(2));
        for layer in body["layers"]
            .as_array()
            .expect("layers should be an array")
        {
            assert_eq!(layer["verified"], true);
            assert_eq!(layer["downloaded_size"], layer["declared_size"]);
            assert!(
                layer["url"]
                    .as_str()
                    .is_some_and(|url| url.contains("/blobs/"))
            );
        }

        let requests = registry.requests();
        assert!(requests.iter().any(|request| request == "token"));
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.ends_with("authorized=false"))
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(
                    |request| request.contains("/blobs/") && request.ends_with("authorized=true")
                )
                .count(),
            3
        );
    }

    #[tokio::test]
    async fn run_accepts_a_direct_platform_manifest() {
        let platform = HostPlatform::current().expect("test host should be supported");
        let registry = RegistryFixture::start(RegistryDocuments::new(
            &platform,
            InitialDocument::Manifest,
            false,
        ))
        .await;

        let (status, body) =
            request_run(OciFetcher::for_test(platform), &registry.manifest_url).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body["index"].is_null());
        assert_eq!(
            body["manifest"]["media_type"],
            "application/vnd.oci.image.manifest.v1+json"
        );
        assert_eq!(body["layers"].as_array().map(Vec::len), Some(2));
    }

    #[tokio::test]
    async fn run_rejects_non_https_registry_urls() {
        let (status, body) = request_run(
            OciFetcher::default(),
            "http://registry.example/v2/library/alpine/manifests/latest",
        )
        .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|error| error.contains("must use HTTPS"))
        );
    }

    #[tokio::test]
    async fn run_rejects_an_index_without_the_host_platform() {
        let platform = HostPlatform::current().expect("test host should be supported");
        let advertised = HostPlatform {
            os: platform.os.clone(),
            architecture: if platform.architecture.as_ref() == "amd64" {
                "arm64".parse().expect("fixture architecture should parse")
            } else {
                "amd64".parse().expect("fixture architecture should parse")
            },
        };
        let registry = RegistryFixture::start(RegistryDocuments::new(
            &advertised,
            InitialDocument::Index,
            false,
        ))
        .await;

        let (status, body) =
            request_run(OciFetcher::for_test(platform), &registry.manifest_url).await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|error| error.contains("image index has no"))
        );
    }

    #[tokio::test]
    async fn run_returns_bad_gateway_when_a_layer_digest_does_not_match() {
        let platform = HostPlatform::current().expect("test host should be supported");
        let registry = RegistryFixture::start(RegistryDocuments::new(
            &platform,
            InitialDocument::Index,
            true,
        ))
        .await;

        let (status, body) =
            request_run(OciFetcher::for_test(platform), &registry.manifest_url).await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|error| error.contains("digest mismatch"))
        );
    }
}
