mod rootfs;

use std::{path::Path, sync::Arc};

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::StatusCode as HttpStatusCode,
};
use barbirolli::{Barbirolli, LifecycleError, Rootfs, VcpuCount, VmId, VmInput, VmStatus};
use dashmap::{
    DashMap,
    mapref::{entry::Entry, one::RefMut},
};
use futures::{FutureExt, TryStreamExt, future::BoxFuture};
use reqwest::{
    Client, Response, StatusCode, Url,
    header::{ACCEPT, CONTENT_TYPE, HeaderMap},
    redirect::Policy,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::{ApiError, AppState, install_ring_crypto_provider};

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

#[derive(derive_more::Debug, derive_more::Display, Clone, Copy)]
#[debug("{_0}")]
#[display("{_0}")]
struct OutboundUrl<'a>(&'a Url);

impl AsRef<Url> for OutboundUrl<'_> {
    fn as_ref(&self) -> &Url {
        self.0
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PullInput {
    name: image::Name,
    tag: image::Tag,
    token: String,
}

impl PullInput {
    fn reference(&self) -> image::Reference {
        image::Reference {
            name: self.name.clone(),
            tag: self.tag.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceInput {
    name: image::Name,
    tag: image::Tag,
}

impl ReferenceInput {
    fn reference(&self) -> image::Reference {
        image::Reference {
            name: self.name.clone(),
            tag: self.tag.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OciStatus {
    Pulled,
    Running,
}

struct FetchedImage {
    details: api_schema::Pull,
    filesystem: rootfs::FileSystem,
}

struct OciEntry {
    image: FetchedImage,
    vm_id: Option<VmId>,
}

impl OciEntry {
    fn response(&self, reference: &image::Reference) -> api_schema::Container {
        api_schema::Container {
            name: reference.name.to_string(),
            tag: reference.tag.to_string(),
            status: if self.vm_id.is_some() {
                OciStatus::Running
            } else {
                OciStatus::Pulled
            },
            id: self.vm_id,
            digest: self.image.details.manifest.digest.to_string(),
            filesystem: (&self.image.filesystem).into(),
        }
    }
}

#[derive(Clone)]
pub struct OciStore {
    fetcher: OciFetcher,
    entries: Arc<DashMap<image::Reference, OciEntry>>,
}

impl Default for OciStore {
    #[tracing::instrument]
    fn default() -> Self {
        tracing::info!("elhone starts to create the OCI store");
        Self {
            fetcher: OciFetcher::default(),
            entries: Arc::new(DashMap::new()),
        }
    }
}

impl OciStore {
    #[cfg(test)]
    #[tracing::instrument(skip(fetcher))]
    fn for_test(fetcher: OciFetcher) -> Self {
        tracing::info!("elhone starts to create the OCI test store");
        Self {
            fetcher,
            entries: Arc::new(DashMap::new()),
        }
    }

    #[tracing::instrument(skip(self, reference, token), fields(%reference), err)]
    async fn ensure_pulled(
        &self,
        reference: &image::Reference,
        token: String,
    ) -> Result<RefMut<'_, image::Reference, OciEntry>, OciStoreError> {
        tracing::info!(%reference, "elhone starts to check the OCI image");
        match self.entries.entry(reference.clone()) {
            Entry::Occupied(entry) => {
                tracing::debug!(%reference, "the OCI image is already in the store");
                Ok(entry.into_ref())
            }
            Entry::Vacant(entry) => {
                let image = self
                    .fetcher
                    .fetch(reference.docker_hub_loc()?, token)
                    .await?;
                tracing::info!(%reference, "elhone stored the OCI image");
                Ok(entry.insert(OciEntry { image, vm_id: None }))
            }
        }
    }

    #[tracing::instrument(skip(self, input), fields(reference = %input.reference()), err)]
    async fn pull(&self, input: PullInput) -> Result<api_schema::Container, OciStoreError> {
        let reference = input.reference();
        tracing::info!(%reference, "elhone starts to pull the OCI image");
        let entry = self.ensure_pulled(&reference, input.token).await?;
        Ok(entry.response(&reference))
    }

    #[tracing::instrument(skip(self, manager, input), fields(reference = %input.reference()), err)]
    async fn run<M: OciVmManager>(
        &self,
        manager: &M,
        input: PullInput,
    ) -> Result<api_schema::Container, OciStoreError> {
        let reference = input.reference();
        tracing::info!(%reference, "elhone starts to run the OCI image");
        let mut entry = self.ensure_pulled(&reference, input.token).await?;

        if let Some(vm_id) = entry.vm_id {
            match manager.start_oci_vm(vm_id).await {
                Ok(()) => {
                    tracing::info!(%reference, %vm_id, "elhone started the OCI VM");
                    return Ok(entry.response(&reference));
                }
                Err(LifecycleError::NotFound(_)) => entry.vm_id = None,
                Err(error) => return Err(error.into()),
            }
        }

        let vm_id = manager
            .create_oci_vm(Rootfs::from(entry.image.filesystem.path.clone()))
            .await?;
        entry.vm_id = Some(vm_id);
        if let Err(start) = manager.start_oci_vm(vm_id).await {
            match manager.delete_oci_vm(vm_id).await {
                Ok(()) => {
                    entry.vm_id = None;
                    return Err(start.into());
                }
                Err(cleanup) => {
                    return Err(OciStoreError::StartCleanup {
                        reference,
                        start: Box::new(start),
                        cleanup: Box::new(cleanup),
                    });
                }
            }
        }
        tracing::info!(%reference, %vm_id, "elhone started the OCI VM");
        Ok(entry.response(&reference))
    }

    #[tracing::instrument(skip(self, manager, input), fields(reference = %input.reference()), err)]
    async fn stop<M: OciVmManager>(
        &self,
        manager: &M,
        input: ReferenceInput,
    ) -> Result<api_schema::Container, OciStoreError> {
        let reference = input.reference();
        tracing::info!(%reference, "elhone starts to stop the OCI VM");
        let mut entry = self
            .entries
            .get_mut(&reference)
            .ok_or_else(|| OciStoreError::NotFound(reference.clone()))?;
        if let Some(vm_id) = entry.vm_id {
            match manager.delete_oci_vm(vm_id).await {
                Ok(()) => {
                    entry.vm_id = None;
                    tracing::info!(%reference, %vm_id, "elhone stopped the OCI VM");
                }
                Err(LifecycleError::NotFound(_)) => {
                    entry.vm_id = None;
                    tracing::debug!(%reference, %vm_id, "the OCI VM is already absent");
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            tracing::debug!(%reference, "the OCI VM is already stopped");
        }
        Ok(entry.response(&reference))
    }

    #[tracing::instrument(skip(self, manager, input), fields(reference = %input.reference()), err)]
    async fn remove<M: OciVmManager>(
        &self,
        manager: &M,
        input: ReferenceInput,
    ) -> Result<(), OciStoreError> {
        let reference = input.reference();
        tracing::info!(%reference, "elhone starts to remove the OCI image");
        let Entry::Occupied(mut stored) = self.entries.entry(reference.clone()) else {
            return Err(OciStoreError::NotFound(reference));
        };
        let entry = stored.get_mut();
        if let Some(vm_id) = entry.vm_id {
            match manager.oci_vm_status(vm_id) {
                Ok(VmStatus::Discovered) => {
                    manager.delete_oci_vm(vm_id).await?;
                    entry.vm_id = None;
                }
                Err(LifecycleError::NotFound(_)) => entry.vm_id = None,
                Ok(_) => return Err(OciStoreError::Running(reference)),
                Err(error) => return Err(error.into()),
            }
        }
        stored.remove();
        tracing::info!(%reference, "elhone removed the OCI image");
        Ok(())
    }
}

trait OciVmManager: Sync {
    fn create_oci_vm(&self, rootfs: Rootfs) -> BoxFuture<'_, Result<VmId, LifecycleError>>;
    fn start_oci_vm(&self, vm_id: VmId) -> BoxFuture<'_, Result<(), LifecycleError>>;
    fn delete_oci_vm(&self, vm_id: VmId) -> BoxFuture<'_, Result<(), LifecycleError>>;
    fn oci_vm_status(&self, vm_id: VmId) -> Result<VmStatus, LifecycleError>;
}

impl OciVmManager for Barbirolli {
    fn create_oci_vm(&self, rootfs: Rootfs) -> BoxFuture<'_, Result<VmId, LifecycleError>> {
        async move {
            self.create(VmInput {
                rootfs,
                provision_ssh_keys: false,
                vcpu_count: VcpuCount::try_from(1).expect("one vCPU is always valid"),
                authorized_keys: Vec::new(),
                bindings: Vec::new(),
            })
            .await
        }
        .boxed()
    }

    fn start_oci_vm(&self, vm_id: VmId) -> BoxFuture<'_, Result<(), LifecycleError>> {
        async move {
            let mut vm = self.vm_mut(vm_id)?;
            vm.start(self).await
        }
        .boxed()
    }

    fn delete_oci_vm(&self, vm_id: VmId) -> BoxFuture<'_, Result<(), LifecycleError>> {
        async move { self.delete(vm_id).await }.boxed()
    }

    fn oci_vm_status(&self, vm_id: VmId) -> Result<VmStatus, LifecycleError> {
        Ok(self.vm(vm_id)?.summary().status)
    }
}

#[derive(Clone)]
pub struct OciFetcher {
    client: Client,
    transport: TransportPolicy,
    platform: Option<platform::HostPlatform>,
    artifacts: rootfs::ArtifactStore,
}

impl Default for OciFetcher {
    #[tracing::instrument]
    fn default() -> Self {
        Self::new(
            TransportPolicy::HttpsOnly,
            None,
            rootfs::ArtifactStore::default(),
        )
    }
}

impl OciFetcher {
    #[tracing::instrument(skip(artifacts))]
    fn new(
        transport: TransportPolicy,
        platform: Option<platform::HostPlatform>,
        artifacts: rootfs::ArtifactStore,
    ) -> Self {
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
            artifacts,
        }
    }

    #[cfg(test)]
    #[tracing::instrument]
    fn for_test(platform: platform::HostPlatform) -> Self {
        Self::new(
            TransportPolicy::HttpLoopback,
            Some(platform),
            rootfs::ArtifactStore::for_test(false),
        )
    }

    #[cfg(test)]
    fn for_test_builder_failure(platform: platform::HostPlatform) -> Self {
        Self::new(
            TransportPolicy::HttpLoopback,
            Some(platform),
            rootfs::ArtifactStore::for_test(true),
        )
    }

    #[tracing::instrument(skip(self, token), fields(url = %loc), err)]
    async fn fetch(&self, loc: registry::Loc, token: String) -> Result<FetchedImage, OciError> {
        if token.trim().is_empty() {
            return Err(OciError::InvalidInput(
                "OCI bearer token must not be empty".to_owned(),
            ));
        }
        let initial_url = self.transport.parse(&loc.manifest)?;
        tracing::info!(
            url = %loc.manifest,
            host = loc.manifest.host_str().unwrap_or_default(),
            "elhone fetches the OCI image"
        );

        let host = match self.platform {
            Some(ref platform) => platform.clone(),
            None => platform::HostPlatform::current()?,
        };
        let mut session = FetchSession::new(&self.client, token);
        let initial = session.fetch_manifest(initial_url, None).await?;

        let (index, selected_platform, manifest) = match initial {
            FetchedManifest::Index {
                document,
                digest,
                media_kind,
            } => {
                let selected = host.select_platform(&document.manifests)?;
                let manifest_url = loc.manifest_url(&selected.digest)?;
                let manifest_url = self.transport.parse(&manifest_url)?;
                let manifest = session
                    .fetch_manifest(manifest_url, Some(&selected))
                    .await?;
                let FetchedManifest::Manifest(manifest) = manifest else {
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
                    manifest,
                )
            }
            FetchedManifest::Manifest(manifest) => (None, None, manifest),
        };

        let config = image::Config::new(&host, &manifest.document.layers, {
            let url = loc.blob_url(&manifest.document.config.digest)?;
            let url = self.transport.parse(&url)?;
            session.fetch_config(url, &manifest.document.config).await?
        })?;

        let platform = config.platform(selected_platform);
        let image = config.response();
        let process_spec = serde_json::to_vec(&config.process()?).map_err(|source| {
            OciError::InvalidDocument(format!("failed to encode OCI process spec: {source}"))
        })?;
        let oci_schema::Rootfs::Layers { diff_ids } = &config.rootfs;
        let (layers, filesystem) = self
            .fetch_layers(
                &mut session,
                &loc,
                &manifest.document.layers,
                diff_ids,
                process_spec,
            )
            .await?;

        Ok(FetchedImage {
            details: api_schema::Pull {
                url: loc.manifest.to_string(),
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
                filesystem: (&filesystem).into(),
            },
            filesystem,
        })
    }

    #[tracing::instrument(
        skip(self, session, loc, descriptors, diff_ids, process_spec),
        fields(layer_count = descriptors.len()),
        err
    )]
    async fn fetch_layers(
        &self,
        session: &mut FetchSession<'_>,
        loc: &registry::Loc,
        descriptors: &[descriptor::Layer],
        diff_ids: &[digest::Sha256Digest],
        process_spec: Vec<u8>,
    ) -> Result<(Vec<api_schema::Layer>, rootfs::FileSystem), OciError> {
        let workspace = rootfs::Workspace::new()?;
        let mut layers = Vec::with_capacity(descriptors.len());
        for (index, (descriptor, diff_id)) in descriptors.iter().zip(diff_ids).enumerate() {
            let layer_url = loc.blob_url(&descriptor.digest)?;
            let layer_url = self.transport.parse(&layer_url)?;
            let downloaded = session
                .fetch_layer(layer_url, descriptor, &workspace.layer_blob(index))
                .await?;
            let applied = self
                .artifacts
                .apply_layer(&workspace, index, &descriptor.media_kind, diff_id.clone())
                .await?;
            layers.push(api_schema::Layer::new(api_schema::LayerInput {
                url: layer_url.as_ref(),
                descriptor,
                downloaded_size: downloaded.size,
                downloaded_digest: downloaded.digest,
                diff_id: applied.diff_id,
                uncompressed_size: applied.uncompressed_size,
            })?);
        }
        let filesystem = self.artifacts.finish(workspace, process_spec).await?;
        Ok((layers, filesystem))
    }
}

#[tracing::instrument(skip(state, input), err)]
pub async fn pull(
    State(state): State<AppState>,
    input: Result<Json<PullInput>, JsonRejection>,
) -> Result<Json<api_schema::Container>, ApiError> {
    tracing::info!("elhone starts the OCI pull request");
    let Json(input) = input.map_err(|error| ApiError::UnprocessableEntity(error.body_text()))?;
    state
        .oci_store
        .pull(input)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

#[tracing::instrument(skip(state, input), err)]
pub async fn run(
    State(state): State<AppState>,
    input: Result<Json<PullInput>, JsonRejection>,
) -> Result<Json<api_schema::Container>, ApiError> {
    tracing::info!("elhone starts the OCI run request");
    let Json(input) = input.map_err(|error| ApiError::UnprocessableEntity(error.body_text()))?;
    state
        .oci_store
        .run(&state.manager, input)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

#[tracing::instrument(skip(state, input), err)]
pub async fn stop(
    State(state): State<AppState>,
    input: Result<Json<ReferenceInput>, JsonRejection>,
) -> Result<Json<api_schema::Container>, ApiError> {
    tracing::info!("elhone starts the OCI stop request");
    let Json(input) = input.map_err(|error| ApiError::UnprocessableEntity(error.body_text()))?;
    state
        .oci_store
        .stop(&state.manager, input)
        .await
        .map(Json)
        .map_err(ApiError::from)
}

#[tracing::instrument(skip(state, input), err)]
pub async fn rm(
    State(state): State<AppState>,
    input: Result<Json<ReferenceInput>, JsonRejection>,
) -> Result<HttpStatusCode, ApiError> {
    tracing::info!("elhone starts the OCI remove request");
    let Json(input) = input.map_err(|error| ApiError::UnprocessableEntity(error.body_text()))?;
    state
        .oci_store
        .remove(&state.manager, input)
        .await
        .map_err(ApiError::from)?;
    Ok(HttpStatusCode::NO_CONTENT)
}

struct FetchSession<'a> {
    client: &'a Client,
    bearer_token: String,
}

impl<'a> FetchSession<'a> {
    #[tracing::instrument(skip(client, bearer_token))]
    fn new(client: &'a Client, bearer_token: String) -> Self {
        Self {
            client,
            bearer_token,
        }
    }

    #[tracing::instrument(skip(self, expected), fields(url = %url), err)]
    async fn fetch_manifest(
        &mut self,
        url: OutboundUrl<'_>,
        expected: Option<&descriptor::Manifest>,
    ) -> Result<FetchedManifest, OciError> {
        let bytes = self
            .fetch_bytes(
                url,
                media::ACCEPT,
                expected.map(|descriptor| descriptor.expectation()),
            )
            .await?;
        let document = serde_json::from_slice::<oci_schema::ManifestDocument>(&bytes.body)
            .map_err(|source| OciError::Json {
                url: url.to_string(),
                source,
            })?;
        match document {
            oci_schema::ManifestDocument::Index(document) => Ok(FetchedManifest::Index {
                digest: bytes.digest,
                media_kind: resolve_media_kind(
                    bytes.content_type.as_deref(),
                    document.media_kind.clone(),
                )?,
                document,
            }),
            oci_schema::ManifestDocument::Manifest(document) => {
                Ok(FetchedManifest::Manifest(FetchedImageManifest {
                    digest: bytes.digest,
                    kind: resolve_media_kind(
                        bytes.content_type.as_deref(),
                        document.media_kind.clone(),
                    )?,
                    document,
                }))
            }
        }
    }

    #[tracing::instrument(skip(self, desc), fields(url = %url, digest = %desc.digest), err)]
    async fn fetch_config(
        &mut self,
        url: OutboundUrl<'_>,
        desc: &descriptor::Config,
    ) -> Result<oci_schema::ImageConfig, OciError> {
        let bytes = self
            .fetch_bytes(url, desc.media_kind.as_ref(), Some(desc.expectation()))
            .await?;
        serde_json::from_slice(&bytes.body).map_err(|source| OciError::Json {
            url: url.to_string(),
            source,
        })
    }

    #[tracing::instrument(skip(self, desc), fields(url = %url, digest = %desc.digest, expected_size = desc.size), err)]
    async fn fetch_layer(
        &mut self,
        url: OutboundUrl<'_>,
        desc: &descriptor::Layer,
        destination: &Path,
    ) -> Result<DownloadedLayer, OciError> {
        let response = self.get(url, desc.media_kind.as_ref()).await?;
        let mut stream = response.bytes_stream();
        let destination_path = destination.to_path_buf();
        let mut destination = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination_path)
            .await
            .map_err(|source| {
                rootfs::Error::io("create downloaded OCI layer", &destination_path, source)
            })?;
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
            destination.write_all(&chunk).await.map_err(|source| {
                rootfs::Error::io("write downloaded OCI layer", &destination_path, source)
            })?;
        }
        destination.flush().await.map_err(|source| {
            rootfs::Error::io("flush downloaded OCI layer", &destination_path, source)
        })?;

        let downloaded = DownloadedLayer {
            size: downloaded_size,
            digest: digest::format(&hasher.finalize())
                .parse()
                .expect("a computed SHA-256 digest should parse"),
        };
        downloaded.verify(url.as_ref(), desc)?;
        Ok(downloaded)
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
        let digest = digest::Sha256Digest::from(body.as_slice());

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
    async fn get(&self, url: OutboundUrl<'_>, accept: &str) -> Result<Response, OciError> {
        ensure_success(url.as_ref(), self.send(url, accept).await?)
    }

    #[tracing::instrument(skip(self), fields(url = %url), err)]
    async fn send(&self, url: OutboundUrl<'_>, accept: &str) -> Result<Response, OciError> {
        self.client
            .get(url.as_ref().clone())
            .header(ACCEPT, accept)
            .bearer_auth(&self.bearer_token)
            .send()
            .await
            .map_err(|source| OciError::Request {
                url: url.to_string(),
                source,
            })
    }
}

#[derive(Debug)]
struct DownloadedLayer {
    size: u64,
    digest: digest::Sha256Digest,
}

impl DownloadedLayer {
    fn verify(&self, url: &Url, descriptor: &descriptor::Layer) -> Result<(), OciError> {
        if descriptor.size != self.size {
            return Err(OciError::SizeMismatch {
                url: url.to_string(),
                expected: descriptor.size,
                actual: self.size,
            });
        }
        if descriptor.digest != self.digest {
            return Err(OciError::DigestMismatch {
                url: url.to_string(),
                expected: descriptor.digest.to_string(),
                actual: self.digest.to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Debug)]
struct FetchedBytesInput<'a> {
    url: &'a Url,
    body: Vec<u8>,
    digest: digest::Sha256Digest,
    size: u64,
    content_type: Option<String>,
    expected: Option<descriptor::BlobExpectation<'a>>,
    header_digest: Option<digest::Sha256Digest>,
}

#[derive(Debug)]
struct FetchedBytes {
    body: Vec<u8>,
    digest: digest::Sha256Digest,
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
enum FetchedManifest {
    Index {
        document: oci_schema::ImageIndex,
        digest: digest::Sha256Digest,
        media_kind: media::IndexMediaKind,
    },
    Manifest(FetchedImageManifest),
}

#[derive(Debug)]
struct FetchedImageManifest {
    document: oci_schema::ImageManifest,
    digest: digest::Sha256Digest,
    kind: media::ManifestMediaKind,
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

    use super::ParseValueError;

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

    use barbirolli::VmId;
    use reqwest::Url;
    use serde::Serialize;

    use crate::oci::{OciStatus, descriptor};

    use super::{digest, media, oci_schema, platform};

    #[derive(Debug, Serialize)]
    pub struct Pull {
        pub url: String,
        pub platform: Platform,
        pub index: Option<Document>,
        pub manifest: Manifest,
        pub image: Image,
        pub rootfs: oci_schema::Rootfs,
        pub history: Vec<oci_schema::History>,
        pub layers: Vec<Layer>,
        pub filesystem: FileSystem,
    }

    #[derive(Debug, Serialize)]
    pub struct Container {
        pub name: String,
        pub tag: String,
        pub status: OciStatus,
        pub id: Option<VmId>,
        pub digest: String,
        pub filesystem: FileSystem,
    }

    #[derive(Debug, Serialize)]
    pub struct Platform {
        pub os: platform::OS,
        pub architecture: platform::Arch,
        pub variant: Option<String>,
        pub os_version: Option<String>,
        pub os_features: Vec<String>,
    }

    #[derive(Debug, Serialize)]
    pub struct Document {
        pub schema_version: u32,
        pub media_kind: media::IndexMediaKind,
        pub digest: digest::Sha256Digest,
        pub annotations: BTreeMap<String, String>,
    }

    #[derive(Debug, Serialize)]
    pub struct Manifest {
        pub schema_version: u32,
        pub media_kind: media::ManifestMediaKind,
        pub digest: digest::Sha256Digest,
        pub annotations: BTreeMap<String, String>,
        pub config: Descriptor,
    }

    #[derive(Debug, Serialize)]
    pub struct Descriptor {
        pub media_kind: media::ConfigMediaKind,
        pub digest: digest::Sha256Digest,
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
        media_kind: media::LayerMediaKind,
        digest: digest::Sha256Digest,
        declared_size: u64,
        downloaded_size: u64,
        diff_id: digest::Sha256Digest,
        uncompressed_size: u64,
    }

    #[derive(Debug)]
    pub struct LayerInput<'a> {
        pub url: &'a Url,
        pub descriptor: &'a descriptor::Layer,
        pub downloaded_size: u64,
        pub downloaded_digest: digest::Sha256Digest,
        pub diff_id: digest::Sha256Digest,
        pub uncompressed_size: u64,
    }

    impl Layer {
        pub fn new(input: LayerInput<'_>) -> Result<Self, super::OciError> {
            if input.descriptor.size != input.downloaded_size {
                return Err(super::OciError::SizeMismatch {
                    url: input.url.to_string(),
                    expected: input.descriptor.size,
                    actual: input.downloaded_size,
                });
            }
            if input.descriptor.digest != input.downloaded_digest {
                return Err(super::OciError::DigestMismatch {
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
                diff_id: input.diff_id,
                uncompressed_size: input.uncompressed_size,
            })
        }
    }

    #[derive(Debug, Serialize)]
    pub struct FileSystem {
        format: &'static str,
        path: std::path::PathBuf,
        size: u64,
        digest: digest::Sha256Digest,
    }

    impl From<&super::rootfs::FileSystem> for FileSystem {
        fn from(filesystem: &super::rootfs::FileSystem) -> Self {
            Self {
                format: "ext4",
                path: filesystem.path.clone(),
                size: filesystem.size,
                digest: filesystem.digest.clone(),
            }
        }
    }
}

mod oci_schema {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};

    use super::{descriptor, digest, media, platform};

    #[derive(Debug, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct PlatformDesc {
        #[serde(rename = "architecture")]
        pub arch: platform::Arch,
        pub os: platform::OS,
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
        #[serde(rename = "mediaType")]
        pub media_kind: Option<media::ManifestMediaKind>,
        pub config: descriptor::Config,
        pub layers: Vec<descriptor::Layer>,
        #[serde(default)]
        pub annotations: BTreeMap<String, String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ImageConfig {
        pub created: Option<String>,
        pub author: Option<String>,
        #[serde(flatten)]
        pub platform: PlatformDesc,
        #[serde(default)]
        pub config: RuntimeConfig,
        pub rootfs: Rootfs,
        #[serde(default)]
        pub history: Vec<History>,
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
        Layers { diff_ids: Vec<digest::Sha256Digest> },
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
    pub enum ManifestDocument {
        Index(ImageIndex),
        Manifest(ImageManifest),
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ImageIndex {
        pub schema_version: u32,
        #[serde(rename = "mediaType")]
        pub media_kind: Option<media::IndexMediaKind>,
        pub manifests: Vec<descriptor::Manifest>,
        #[serde(default)]
        pub annotations: BTreeMap<String, String>,
    }
}

mod descriptor {
    use serde::Deserialize;

    use super::{digest, media, oci_schema};

    #[derive(Debug, Clone, Deserialize)]
    #[serde(rename_all = "camelCase", bound(deserialize = ""))]
    pub struct Descriptor<K: media::MediaTypeKind> {
        #[serde(rename = "mediaType")]
        pub media_kind: media::MediaKind<K>,
        pub digest: digest::Sha256Digest,
        pub size: u64,
        #[serde(default)]
        pub platform: Option<oci_schema::PlatformDesc>,
    }

    impl<K: media::MediaTypeKind> Descriptor<K> {
        pub fn expectation(&self) -> BlobExpectation<'_> {
            BlobExpectation {
                digest: &self.digest,
                size: self.size,
            }
        }
    }

    pub type Manifest = Descriptor<media::ManifestMedia>;
    pub type Config = Descriptor<media::ConfigMedia>;
    pub type Layer = Descriptor<media::LayerMedia>;

    #[derive(Debug, Clone, Copy)]
    pub struct BlobExpectation<'a> {
        pub digest: &'a digest::Sha256Digest,
        pub size: u64,
    }
}

mod image {
    use std::{
        fmt::{self, Display},
        str::FromStr,
    };

    use oci_spec::runtime::Process;
    use serde::{Deserialize, Deserializer, de};
    use serde_json::json;

    use crate::oci::{OciError, registry};

    use super::{api_schema, descriptor, oci_schema, platform};

    type Result<T, E = ParseImageConfigError> = std::result::Result<T, E>;

    #[derive(Debug)]
    pub struct Config {
        pub created: Option<String>,
        pub author: Option<String>,
        platform: Platform,
        pub runtime: oci_schema::RuntimeConfig,
        pub rootfs: oci_schema::Rootfs,
        pub history: Vec<oci_schema::History>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub struct Reference {
        pub name: Name,
        pub tag: Tag,
    }

    impl Reference {
        pub fn docker_hub_loc(&self) -> Result<registry::Loc, OciError> {
            registry::Loc::docker_hub(&self.name, &self.tag)
        }
    }

    impl Config {
        pub fn new(
            host: &platform::HostPlatform,
            layers: &[descriptor::Layer],
            doc: oci_schema::ImageConfig,
        ) -> Result<Self> {
            let platform = Platform::new(doc.platform, host)?;
            let oci_schema::Rootfs::Layers { diff_ids } = &doc.rootfs;
            if diff_ids.len() != layers.len() {
                return Err(ParseImageConfigError::Rootfs {
                    diff_id_count: diff_ids.len(),
                    layer_count: layers.len(),
                });
            }

            Ok(Self {
                created: doc.created,
                author: doc.author,
                platform,
                runtime: doc.config,
                rootfs: doc.rootfs,
                history: doc.history,
            })
        }

        #[tracing::instrument(skip(self, selected))]
        pub fn platform(&self, selected: Option<oci_schema::PlatformDesc>) -> api_schema::Platform {
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
        pub fn response(&self) -> api_schema::Image {
            let runtime = &self.runtime;
            api_schema::Image {
                created: self.created.clone(),
                author: self.author.clone(),
                user: runtime.user.clone(),
                exposed_ports: runtime.exposed_ports.keys().cloned().collect(),
                env: runtime.env.clone(),
                entrypoint: runtime.entrypoint.clone(),
                cmd: runtime.cmd.clone(),
                volumes: runtime.volumes.keys().cloned().collect(),
                working_dir: runtime.working_dir.clone(),
                labels: runtime.labels.clone(),
                stop_signal: runtime.stop_signal.clone(),
                args_escaped: runtime.args_escaped,
            }
        }

        pub fn process(&self) -> Result<Process> {
            let mut args = self.runtime.entrypoint.clone();
            args.extend(self.runtime.cmd.clone());
            if args.is_empty() || args.first().is_some_and(String::is_empty) {
                return Err(ParseImageConfigError::Runtime(
                    "image has no executable entrypoint or command".to_owned(),
                ));
            }
            for entry in &self.runtime.env {
                let Some((name, _)) = entry.split_once('=') else {
                    return Err(ParseImageConfigError::Runtime(format!(
                        "image environment entry {entry:?} must use NAME=VALUE"
                    )));
                };
                if name.is_empty() || name.as_bytes().contains(&0) {
                    return Err(ParseImageConfigError::Runtime(format!(
                        "image environment entry {entry:?} has an invalid name"
                    )));
                }
            }
            let (uid, gid) = numeric_user(self.runtime.user.as_deref())?;
            let cwd = self
                .runtime
                .working_dir
                .as_deref()
                .filter(|cwd| !cwd.is_empty())
                .unwrap_or("/");
            if !cwd.starts_with('/') {
                return Err(ParseImageConfigError::Runtime(format!(
                    "image working directory {cwd:?} must be absolute"
                )));
            }
            serde_json::from_value(json!({
                "terminal": false,
                "user": {
                    "uid": uid,
                    "gid": gid
                },
                "args": args,
                "env": self.runtime.env,
                "cwd": cwd,
                "noNewPrivileges": false
            }))
            .map_err(|error| {
                ParseImageConfigError::Runtime(format!(
                    "failed to build OCI process configuration: {error}"
                ))
            })
        }
    }

    impl Display for Reference {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "{}:{}", self.name, self.tag)
        }
    }

    #[derive(derive_more::Debug, derive_more::Display, Clone, PartialEq, Eq, Hash)]
    #[debug("{_0}")]
    #[display("{_0}")]
    pub struct Name(String);

    impl FromStr for Name {
        type Err = OciError;

        fn from_str(value: &str) -> Result<Self, Self::Err> {
            let valid_component = |component: &str| {
                !component.is_empty()
                    && component.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
                    })
                    && component
                        .as_bytes()
                        .first()
                        .is_some_and(u8::is_ascii_alphanumeric)
                    && component
                        .as_bytes()
                        .last()
                        .is_some_and(u8::is_ascii_alphanumeric)
            };
            if value.is_empty() || !value.split('/').all(valid_component) {
                return Err(OciError::InvalidInput(format!(
                    "invalid OCI image name {value:?}"
                )));
            }
            Ok(Self(if value.contains('/') {
                value.to_owned()
            } else {
                format!("library/{value}")
            }))
        }
    }

    impl<'de> Deserialize<'de> for Name {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            String::deserialize(deserializer)?
                .parse()
                .map_err(de::Error::custom)
        }
    }

    #[derive(derive_more::Debug, derive_more::Display, Clone, PartialEq, Eq, Hash)]
    #[debug("{_0}")]
    #[display("{_0}")]
    pub struct Tag(String);

    impl FromStr for Tag {
        type Err = OciError;

        fn from_str(value: &str) -> Result<Self, Self::Err> {
            let valid = !value.is_empty()
                && value.len() <= 128
                && value
                    .as_bytes()
                    .first()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte));
            valid
                .then(|| Self(value.to_owned()))
                .ok_or_else(|| OciError::InvalidInput(format!("invalid OCI image tag {value:?}")))
        }
    }

    impl<'de> Deserialize<'de> for Tag {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            String::deserialize(deserializer)?
                .parse()
                .map_err(de::Error::custom)
        }
    }

    fn numeric_user(value: Option<&str>) -> Result<(u32, u32)> {
        let value = value.unwrap_or_default().trim();
        if value.is_empty() {
            return Ok((0, 0));
        }
        let (uid, gid) = value.split_once(':').map_or((value, value), |parts| parts);
        let parse = |part: &str, kind: &str| {
            part.parse::<u32>().map_err(|_| {
                ParseImageConfigError::Runtime(format!(
                    "image user {value:?} has a non-numeric {kind}"
                ))
            })
        };
        Ok((parse(uid, "UID")?, parse(gid, "GID")?))
    }

    #[derive(Debug)]
    struct Platform {
        architecture: platform::Arch,
        os: platform::OS,
        variant: Option<String>,
        os_version: Option<String>,
        os_features: Vec<String>,
    }

    impl Platform {
        fn new(platform: oci_schema::PlatformDesc, host: &platform::HostPlatform) -> Result<Self> {
            if platform.os != host.os || platform.arch != host.architecture {
                return Err(ParseImageConfigError::Platform {
                    image_os: platform.os,
                    image_arch: platform.arch,
                    host_os: host.os.clone(),
                    host_arch: host.architecture.clone(),
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
        #[error("image platform {image_os}/{image_arch} does not match host {host_os}/{host_arch}")]
        Platform {
            image_os: platform::OS,
            image_arch: platform::Arch,
            host_os: platform::OS,
            host_arch: platform::Arch,
        },
        #[error("rootfs has {diff_id_count} diff IDs but manifest has {layer_count} layers")]
        Rootfs {
            diff_id_count: usize,
            layer_count: usize,
        },
        #[error("unsupported OCI runtime configuration: {0}")]
        Runtime(String),
    }
}

mod registry {
    use std::{
        fmt::{self, Display},
        str::FromStr,
    };

    use reqwest::Url;
    use serde::{Deserialize, Deserializer, de};

    use crate::oci::image;

    use super::{OciError, digest};

    #[derive(Debug)]
    pub struct Loc {
        pub manifest: Url,
    }

    impl Loc {
        pub fn docker_hub(name: &image::Name, tag: &image::Tag) -> Result<Self, OciError> {
            format!("https://registry-1.docker.io/v2/{name}/manifests/{tag}").parse()
        }
    }

    impl FromStr for Loc {
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

    impl<'de> Deserialize<'de> for Loc {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            String::deserialize(deserializer)?
                .parse()
                .map_err(de::Error::custom)
        }
    }

    impl Display for Loc {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.manifest.fmt(formatter)
        }
    }

    impl Loc {
        #[tracing::instrument(err)]
        pub fn manifest_url(&self, digest: &digest::Sha256Digest) -> Result<Url, OciError> {
            self.descriptor_url("manifests", digest)
        }

        #[tracing::instrument(err)]
        pub fn blob_url(&self, digest: &digest::Sha256Digest) -> Result<Url, OciError> {
            self.descriptor_url("blobs", digest)
        }

        #[tracing::instrument(err)]
        fn descriptor_url(
            &self,
            kind: &str,
            digest: &digest::Sha256Digest,
        ) -> Result<Url, OciError> {
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
    use std::str::FromStr;

    use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
    use sha2::{Digest as _, Sha256};

    use super::ParseValueError;

    #[derive(derive_more::Debug, derive_more::Display, Clone, PartialEq, Eq, Hash)]
    #[debug("{_0}")]
    #[display("{_0}")]
    pub struct Sha256Digest(String);

    impl From<&[u8]> for Sha256Digest {
        #[tracing::instrument(skip(bytes), fields(byte_count = bytes.len()))]
        fn from(bytes: &[u8]) -> Sha256Digest {
            Sha256Digest(format(&Sha256::digest(bytes)))
        }
    }

    #[tracing::instrument(skip(bytes))]
    pub fn format(bytes: &[u8]) -> String {
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
    use std::str::FromStr;

    use serde::{Deserialize, Deserializer, Serialize, de};

    use super::{OciError, ParseValueError, descriptor};

    #[derive(Debug, Clone)]
    pub struct HostPlatform {
        pub os: OS,
        pub architecture: Arch,
    }

    impl HostPlatform {
        #[tracing::instrument(err)]
        pub fn current() -> Result<Self, OciError> {
            let os = match std::env::consts::OS {
                "macos" => "darwin",
                os => os,
            };
            let arch = match std::env::consts::ARCH {
                "x86_64" => "amd64",
                "aarch64" => "arm64",
                arch => {
                    return Err(OciError::InvalidInput(format!(
                        "unsupported host architecture {arch:?}"
                    )));
                }
            };
            Ok(Self {
                os: os
                    .parse()
                    .map_err(|error: ParseValueError| OciError::InvalidInput(error.to_string()))?,
                architecture: arch
                    .parse()
                    .map_err(|error: ParseValueError| OciError::InvalidInput(error.to_string()))?,
            })
        }

        #[tracing::instrument(skip(manifests), fields(os = %self.os, architecture = %self.architecture), err)]
        pub fn select_platform(
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

    #[derive(derive_more::Debug, derive_more::Display, Clone, PartialEq, Eq, Hash, Serialize)]
    #[debug("{_0}")]
    #[display("{_0}")]
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

    #[derive(derive_more::Debug, derive_more::Display, Clone, PartialEq, Eq, Hash, Serialize)]
    #[debug("{_0}")]
    #[display("{_0}")]
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
enum OciStoreError {
    #[error(transparent)]
    Fetch(#[from] OciError),
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    #[error("OCI image {0} is not pulled")]
    NotFound(image::Reference),
    #[error("OCI image {0} must be stopped before removal")]
    Running(image::Reference),
    #[error("failed to start OCI image {reference}: {start}; cleanup also failed: {cleanup}")]
    StartCleanup {
        reference: image::Reference,
        start: Box<LifecycleError>,
        cleanup: Box<LifecycleError>,
    },
}

impl From<OciStoreError> for ApiError {
    fn from(error: OciStoreError) -> Self {
        match error {
            OciStoreError::Fetch(error) => Self::from(error),
            OciStoreError::Lifecycle(error) => Self::from(error),
            OciStoreError::NotFound(_) => Self::NotFound(error.to_string()),
            OciStoreError::Running(_) => Self::Conflict(error.to_string()),
            OciStoreError::StartCleanup { .. } => {
                tracing::error!(%error, "the OCI VM startup rollback failed");
                Self::InternalServerError(error.to_string())
            }
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
    #[error(transparent)]
    Rootfs(#[from] rootfs::Error),
}

impl OciError {
    pub fn is_local_failure(&self) -> bool {
        matches!(self, Self::Rootfs(error) if error.is_local())
    }
}

impl From<ParseValueError> for OciError {
    fn from(error: ParseValueError) -> Self {
        Self::InvalidDocument(error.to_string())
    }
}

impl From<image::ParseImageConfigError> for OciError {
    fn from(error: image::ParseImageConfigError) -> Self {
        let message = error.to_string();
        match error {
            image::ParseImageConfigError::Platform { .. }
            | image::ParseImageConfigError::Runtime(_) => Self::InvalidInput(message),
            image::ParseImageConfigError::Rootfs { .. } => Self::InvalidDocument(message),
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
    use std::{
        collections::HashMap,
        io::Write,
        path::Path,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU16, Ordering},
        },
    };

    use axum::{
        Router,
        body::Body,
        extract::State,
        http::{
            HeaderMap, StatusCode, Uri,
            header::{AUTHORIZATION, CONTENT_TYPE},
        },
        response::{IntoResponse, Response},
        routing::get,
    };
    use barbirolli::{LifecycleError, Rootfs, VmId, VmStatus};
    use flate2::{Compression as GzipCompression, write::GzEncoder};
    use futures::{FutureExt, future::BoxFuture};
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::{
        ApiError, FetchedImage, OciEntry, OciFetcher, OciStatus, OciStore, OciStoreError,
        OciVmManager, PullInput, ReferenceInput, descriptor, digest, image, oci_schema, platform,
    };

    const TOKEN: &str = "fixture-token";

    #[test]
    fn ring_crypto_provider_installation_is_idempotent() {
        crate::install_ring_crypto_provider();
        crate::install_ring_crypto_provider();
        let _fetcher = OciFetcher::default();
    }

    #[derive(Debug, Clone, Copy)]
    enum InitialDocument {
        Index,
        Manifest,
    }

    #[derive(Debug, Clone, Copy)]
    enum Corruption {
        None,
        CompressedDigest,
        DiffId,
    }

    #[derive(Debug, Clone, Copy)]
    enum LayerEncoding {
        Gzip,
        Zstd,
    }

    #[derive(Default)]
    struct FakeManager {
        next_id: AtomicU16,
        states: Mutex<HashMap<VmId, VmStatus>>,
        rootfs: Mutex<Vec<Rootfs>>,
    }

    impl OciVmManager for FakeManager {
        fn create_oci_vm(&self, rootfs: Rootfs) -> BoxFuture<'_, Result<VmId, LifecycleError>> {
            async move {
                let id = VmId::try_from(self.next_id.fetch_add(1, Ordering::Relaxed))
                    .expect("fixture VM ID should be valid");
                self.states
                    .lock()
                    .expect("fixture states should not be poisoned")
                    .insert(id, VmStatus::Discovered);
                self.rootfs
                    .lock()
                    .expect("fixture rootfs list should not be poisoned")
                    .push(rootfs);
                Ok(id)
            }
            .boxed()
        }

        fn start_oci_vm(&self, vm_id: VmId) -> BoxFuture<'_, Result<(), LifecycleError>> {
            async move {
                let mut registry = self
                    .states
                    .lock()
                    .expect("fixture states should not be poisoned");
                let status = registry
                    .get_mut(&vm_id)
                    .ok_or(LifecycleError::NotFound(vm_id))?;
                *status = VmStatus::Running;
                Ok(())
            }
            .boxed()
        }

        fn delete_oci_vm(&self, vm_id: VmId) -> BoxFuture<'_, Result<(), LifecycleError>> {
            async move {
                self.states
                    .lock()
                    .expect("fixture states should not be poisoned")
                    .remove(&vm_id)
                    .map(|_| ())
                    .ok_or(LifecycleError::NotFound(vm_id))
            }
            .boxed()
        }

        fn oci_vm_status(&self, vm_id: VmId) -> Result<VmStatus, LifecycleError> {
            self.states
                .lock()
                .expect("fixture states should not be poisoned")
                .get(&vm_id)
                .copied()
                .ok_or(LifecycleError::NotFound(vm_id))
        }
    }

    #[derive(Debug)]
    struct LayerFixture {
        body: Vec<u8>,
        digest: digest::Sha256Digest,
        diff_id: digest::Sha256Digest,
        media_type: &'static str,
    }

    impl LayerFixture {
        fn new(path: &str, body: &[u8], encoding: LayerEncoding) -> Self {
            let mut archive = Vec::new();
            {
                let mut builder = tar::Builder::new(&mut archive);
                let mut header = tar::Header::new_gnu();
                header.set_path(path).expect("fixture path should be valid");
                header.set_size(u64::try_from(body.len()).expect("fixture body should fit in u64"));
                header.set_mode(0o644);
                header.set_uid(0);
                header.set_gid(0);
                header.set_mtime(1);
                header.set_cksum();
                builder
                    .append(&header, body)
                    .expect("fixture tar entry should append");
                builder.finish().expect("fixture tar should finish");
            }
            let diff_id = digest::Sha256Digest::from(archive.as_slice());
            let (body, media_type) = match encoding {
                LayerEncoding::Gzip => {
                    let mut encoder = GzEncoder::new(Vec::new(), GzipCompression::default());
                    encoder
                        .write_all(&archive)
                        .expect("fixture gzip encoder should write");
                    (
                        encoder
                            .finish()
                            .expect("fixture gzip encoder should finish"),
                        "application/vnd.oci.image.layer.v1.tar+gzip",
                    )
                }
                LayerEncoding::Zstd => (
                    zstd::stream::encode_all(archive.as_slice(), 0)
                        .expect("fixture zstd encoder should finish"),
                    "application/vnd.oci.image.layer.v1.tar+zstd",
                ),
            };
            let digest = digest::Sha256Digest::from(body.as_slice());
            Self {
                body,
                digest,
                diff_id,
                media_type,
            }
        }
    }

    #[derive(Debug, Clone)]
    struct ServedDocument {
        body: Vec<u8>,
        media_type: &'static str,
        digest: digest::Sha256Digest,
    }

    impl ServedDocument {
        fn from_json(value: &Value, media_type: &'static str) -> Self {
            let body = serde_json::to_vec(value).expect("fixture JSON should serialize");
            let digest = digest::Sha256Digest::from(body.as_slice());
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
        layers: Vec<(digest::Sha256Digest, &'static str, Vec<u8>)>,
    }

    impl RegistryDocuments {
        fn new(
            platform: &platform::HostPlatform,
            initial: InitialDocument,
            corruption: Corruption,
        ) -> Self {
            let first_layer = LayerFixture::new(
                "etc/first-layer",
                b"first filesystem layer",
                LayerEncoding::Gzip,
            );
            let second_layer = LayerFixture::new(
                "etc/second-layer",
                b"second filesystem layer",
                LayerEncoding::Zstd,
            );
            let first_digest = if matches!(corruption, Corruption::CompressedDigest) {
                format!("sha256:{}", "0".repeat(64))
                    .parse()
                    .expect("fixture digest should be valid")
            } else {
                first_layer.digest.clone()
            };
            let first_diff_id = if matches!(corruption, Corruption::DiffId) {
                format!("sha256:{}", "0".repeat(64))
                    .parse()
                    .expect("fixture diff ID should be valid")
            } else {
                first_layer.diff_id.clone()
            };

            let config = Self::config_document(platform, &first_diff_id, &second_layer.diff_id);
            let manifest =
                Self::manifest_document(&config, &first_layer, &first_digest, &second_layer);
            let index = Self::index_document(platform, &manifest);
            let initial = match initial {
                InitialDocument::Index => index,
                InitialDocument::Manifest => manifest.clone(),
            };
            Self {
                initial,
                manifest,
                config,
                layers: vec![
                    (first_digest, first_layer.media_type, first_layer.body),
                    (
                        second_layer.digest,
                        second_layer.media_type,
                        second_layer.body,
                    ),
                ],
            }
        }

        fn config_document(
            platform: &platform::HostPlatform,
            first_diff_id: &digest::Sha256Digest,
            second_diff_id: &digest::Sha256Digest,
        ) -> ServedDocument {
            ServedDocument::from_json(
                &json!({
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
                            first_diff_id,
                            second_diff_id
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
            )
        }

        fn manifest_document(
            config: &ServedDocument,
            first_layer: &LayerFixture,
            first_digest: &digest::Sha256Digest,
            second_layer: &LayerFixture,
        ) -> ServedDocument {
            ServedDocument::from_json(
                &json!({
                    "schemaVersion": 2,
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "config": {
                        "mediaType": "application/vnd.oci.image.config.v1+json",
                        "digest": config.digest,
                        "size": config.body.len()
                    },
                    "layers": [
                        {
                            "mediaType": first_layer.media_type,
                            "digest": first_digest,
                            "size": first_layer.body.len()
                        },
                        {
                            "mediaType": second_layer.media_type,
                            "digest": second_layer.digest,
                            "size": second_layer.body.len()
                        }
                    ],
                    "annotations": {
                        "org.opencontainers.image.ref.name": "latest"
                    }
                }),
                "application/vnd.oci.image.manifest.v1+json",
            )
        }

        fn index_document(
            platform: &platform::HostPlatform,
            manifest: &ServedDocument,
        ) -> ServedDocument {
            ServedDocument::from_json(
                &json!({
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
            )
        }
    }

    #[derive(Clone)]
    struct RegistryState {
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
                documents: Arc::new(documents),
                requests: requests.clone(),
            };
            let app = Router::new()
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
            return StatusCode::UNAUTHORIZED.into_response();
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

    fn bytes_response(body: &[u8], media_type: &str, digest: &digest::Sha256Digest) -> Response {
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, media_type)
            .header("docker-content-digest", digest.as_ref())
            .body(Body::from(body.to_vec()))
            .expect("fixture document response should build")
    }

    async fn request_pull(
        fetcher: &OciFetcher,
        url: &str,
        token: &str,
    ) -> (StatusCode, Value, Option<FetchedImage>) {
        let location = url.parse().expect("fixture registry location should parse");
        match fetcher.fetch(location, token.to_owned()).await {
            Ok(image) => {
                let body =
                    serde_json::to_value(&image.details).expect("pull response should serialize");
                (StatusCode::OK, body, Some(image))
            }
            Err(error) => {
                let error = ApiError::from(error);
                (error.status(), json!({ "error": error.to_string() }), None)
            }
        }
    }

    fn layer_descriptors(count: usize) -> Vec<descriptor::Layer> {
        (0..count)
            .map(|index| descriptor::Descriptor {
                media_kind: "application/vnd.oci.image.layer.v1.tar"
                    .parse()
                    .expect("fixture media type should parse"),
                digest: digest::Sha256Digest::from(index.to_string().as_bytes()),
                size: 0,
                platform: None,
            })
            .collect()
    }

    fn configuration_document(
        platform: &platform::HostPlatform,
        diff_id_count: usize,
    ) -> oci_schema::ImageConfig {
        let diff_ids = (0..diff_id_count)
            .map(|index| digest::Sha256Digest::from(index.to_string().as_bytes()))
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

    fn fixture_reference() -> image::Reference {
        image::Reference {
            name: "fixture".parse::<image::Name>().expect("valid image name"),
            tag: "latest".parse::<image::Tag>().expect("valid image tag"),
        }
    }

    fn pull_input() -> PullInput {
        let reference = fixture_reference();
        PullInput {
            name: reference.name,
            tag: reference.tag,
            token: TOKEN.to_owned(),
        }
    }

    fn reference_input() -> ReferenceInput {
        let reference = fixture_reference();
        ReferenceInput {
            name: reference.name,
            tag: reference.tag,
        }
    }

    #[test]
    fn platform_fields_parse_during_deserialization() {
        let platform = platform::HostPlatform::current().expect("test host should be supported");
        let error = serde_json::from_value::<oci_schema::ImageConfig>(json!({
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
    fn image_reference_is_indexed_by_normalized_name_and_tag() {
        let latest = serde_json::from_value::<ReferenceInput>(json!({
            "name": "alpine",
            "tag": "latest"
        }))
        .expect("valid image reference should parse")
        .reference();
        let versioned = serde_json::from_value::<ReferenceInput>(json!({
            "name": "alpine",
            "tag": "3.22"
        }))
        .expect("valid image reference should parse")
        .reference();

        assert_eq!(latest.name.to_string(), "library/alpine");
        assert_eq!(latest.tag.to_string(), "latest");
        assert_ne!(latest, versioned);
    }

    #[test]
    fn image_configuration_parser_rejects_a_different_platform() {
        let platform = platform::HostPlatform::current().expect("test host should be supported");
        let mut document = configuration_document(&platform, 1);
        document.platform.arch = if platform.architecture.as_ref() == "amd64" {
            "arm64".parse().expect("fixture architecture should parse")
        } else {
            "amd64".parse().expect("fixture architecture should parse")
        };

        let error = image::Config::new(&platform, &layer_descriptors(1), document)
            .expect_err("a different platform must not produce an image configuration");

        assert!(matches!(
            error,
            image::ParseImageConfigError::Platform { .. }
        ));
    }

    #[test]
    fn image_configuration_parser_rejects_a_rootfs_for_different_layers() {
        let platform = platform::HostPlatform::current().expect("test host should be supported");
        let document = configuration_document(&platform, 1);

        let error = image::Config::new(&platform, &layer_descriptors(2), document)
            .expect_err("a mismatched rootfs must not produce an image configuration");

        assert!(matches!(
            error,
            image::ParseImageConfigError::Rootfs {
                diff_id_count: 1,
                layer_count: 2,
            }
        ));
    }

    #[test]
    fn image_runtime_configuration_becomes_the_guest_process() {
        let platform = platform::HostPlatform::current().expect("test host should be supported");
        let mut document = configuration_document(&platform, 1);
        document.config.user = Some("1000:1001".to_owned());
        document.config.entrypoint = vec!["/bin/fixture".to_owned()];
        document.config.cmd = vec!["serve".to_owned()];
        document.config.env = vec!["MESSAGE=hello".to_owned()];
        document.config.working_dir = Some("/srv".to_owned());
        let config = image::Config::new(&platform, &layer_descriptors(1), document)
            .expect("image configuration should parse");

        let process = config.process().expect("guest process should build");

        assert_eq!(
            process.args().as_deref(),
            Some(["/bin/fixture".to_owned(), "serve".to_owned()].as_slice())
        );
        assert_eq!(
            process.env().as_deref(),
            Some(["MESSAGE=hello".to_owned()].as_slice())
        );
        assert_eq!(process.cwd(), Path::new("/srv"));
        assert_eq!(process.user().uid(), 1000);
        assert_eq!(process.user().gid(), 1001);
        assert_eq!(process.no_new_privileges(), Some(false));
    }

    #[tokio::test]
    async fn pull_forwards_token_and_verifies_every_layer() {
        let platform = platform::HostPlatform::current().expect("test host should be supported");
        let registry = RegistryFixture::start(RegistryDocuments::new(
            &platform,
            InitialDocument::Index,
            Corruption::None,
        ))
        .await;

        let fetcher = OciFetcher::for_test(platform.clone());
        let (status, body, image) = request_pull(&fetcher, &registry.manifest_url, TOKEN).await;
        let image = image.expect("successful pull should retain its artifact");

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
            body["index"]["media_kind"],
            "application/vnd.oci.image.index.v1+json"
        );
        assert_eq!(
            body["manifest"]["media_kind"],
            "application/vnd.oci.image.manifest.v1+json"
        );
        assert_eq!(
            body["manifest"]["config"]["media_kind"],
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
            assert_eq!(layer["downloaded_size"], layer["declared_size"]);
            assert!(
                layer["diff_id"]
                    .as_str()
                    .is_some_and(|value| value.starts_with("sha256:"))
            );
            assert!(
                layer["uncompressed_size"]
                    .as_u64()
                    .is_some_and(|size| size > 0)
            );
            assert!(
                layer["url"]
                    .as_str()
                    .is_some_and(|url| url.contains("/blobs/"))
            );
        }
        assert_eq!(body["filesystem"]["format"], "ext4");
        let filesystem = body["filesystem"]["path"]
            .as_str()
            .expect("filesystem path should be a string");
        assert!(Path::new(filesystem).is_file());
        assert!(
            body["filesystem"]["size"]
                .as_u64()
                .is_some_and(|size| size > 0)
        );
        assert!(
            body["filesystem"]["digest"]
                .as_str()
                .is_some_and(|value| value.starts_with("sha256:"))
        );
        assert_eq!(
            image.filesystem.artifact_dir(),
            Path::new(filesystem)
                .parent()
                .expect("filesystem should have a parent")
        );

        let requests = registry.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.ends_with("authorized=false"))
                .count(),
            0
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
    async fn pull_accepts_a_direct_platform_manifest() {
        let platform = platform::HostPlatform::current().expect("test host should be supported");
        let registry = RegistryFixture::start(RegistryDocuments::new(
            &platform,
            InitialDocument::Manifest,
            Corruption::None,
        ))
        .await;

        let fetcher = OciFetcher::for_test(platform);
        let (status, body, _image) = request_pull(&fetcher, &registry.manifest_url, TOKEN).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body["index"].is_null());
        assert_eq!(
            body["manifest"]["media_kind"],
            "application/vnd.oci.image.manifest.v1+json"
        );
        assert_eq!(body["layers"].as_array().map(Vec::len), Some(2));
    }

    #[tokio::test]
    async fn pull_rejects_non_https_registry_urls() {
        let fetcher = OciFetcher::default();
        let (status, body, _image) = request_pull(
            &fetcher,
            "http://registry.example/v2/library/alpine/manifests/latest",
            TOKEN,
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
    async fn pull_rejects_an_index_without_the_host_platform() {
        let platform = platform::HostPlatform::current().expect("test host should be supported");
        let advertised = platform::HostPlatform {
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
            Corruption::None,
        ))
        .await;

        let fetcher = OciFetcher::for_test(platform);
        let (status, body, _image) = request_pull(&fetcher, &registry.manifest_url, TOKEN).await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|error| error.contains("image index has no"))
        );
    }

    #[tokio::test]
    async fn pull_returns_bad_gateway_when_a_layer_digest_does_not_match() {
        let platform = platform::HostPlatform::current().expect("test host should be supported");
        let registry = RegistryFixture::start(RegistryDocuments::new(
            &platform,
            InitialDocument::Index,
            Corruption::CompressedDigest,
        ))
        .await;

        let fetcher = OciFetcher::for_test(platform);
        let (status, body, image) = request_pull(&fetcher, &registry.manifest_url, TOKEN).await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|error| error.contains("digest mismatch"))
        );
        assert!(image.is_none());
    }

    #[tokio::test]
    async fn pull_returns_bad_gateway_when_a_layer_diff_id_does_not_match() {
        let platform = platform::HostPlatform::current().expect("test host should be supported");
        let registry = RegistryFixture::start(RegistryDocuments::new(
            &platform,
            InitialDocument::Index,
            Corruption::DiffId,
        ))
        .await;
        let fetcher = OciFetcher::for_test(platform);

        let (status, body, image) = request_pull(&fetcher, &registry.manifest_url, TOKEN).await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|error| error.contains("diff ID mismatch"))
        );
        assert!(image.is_none());
    }

    #[tokio::test]
    async fn pull_returns_internal_server_error_when_ext4_building_fails() {
        let platform = platform::HostPlatform::current().expect("test host should be supported");
        let registry = RegistryFixture::start(RegistryDocuments::new(
            &platform,
            InitialDocument::Index,
            Corruption::None,
        ))
        .await;
        let fetcher = OciFetcher::for_test_builder_failure(platform);

        let (status, body, image) = request_pull(&fetcher, &registry.manifest_url, TOKEN).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|error| error.contains("filesystem builder failed"))
        );
        assert!(image.is_none());
    }

    #[tokio::test]
    async fn store_runs_stops_and_removes_one_vm_per_name_and_tag() {
        let platform = platform::HostPlatform::current().expect("test host should be supported");
        let registry = RegistryFixture::start(RegistryDocuments::new(
            &platform,
            InitialDocument::Index,
            Corruption::None,
        ))
        .await;
        let store = OciStore::for_test(OciFetcher::for_test(platform));
        let image = store
            .fetcher
            .fetch(
                registry
                    .manifest_url
                    .parse()
                    .expect("fixture registry location should parse"),
                TOKEN.to_owned(),
            )
            .await
            .expect("fixture image should pull");
        let filesystem = image.filesystem.path.clone();
        store
            .entries
            .insert(fixture_reference(), OciEntry { image, vm_id: None });
        let manager = FakeManager::default();

        let running = store
            .run(&manager, pull_input())
            .await
            .expect("pulled image should run");
        let repeated = store
            .run(&manager, pull_input())
            .await
            .expect("running image should be idempotent");
        assert_eq!(running.status, OciStatus::Running);
        assert_eq!(repeated.id, running.id);
        assert_eq!(
            manager
                .rootfs
                .lock()
                .expect("fixture rootfs list should not be poisoned")
                .len(),
            1
        );

        let error = store
            .remove(&manager, reference_input())
            .await
            .expect_err("running image must not be removable");
        assert!(matches!(error, OciStoreError::Running(_)));
        assert!(filesystem.exists());

        let stopped = store
            .stop(&manager, reference_input())
            .await
            .expect("running image should stop");
        assert_eq!(stopped.status, OciStatus::Pulled);
        assert!(stopped.id.is_none());
        assert!(filesystem.exists());

        store
            .remove(&manager, reference_input())
            .await
            .expect("stopped image should be removable");
        assert!(store.entries.is_empty());
        assert!(!filesystem.exists());
    }

    #[tokio::test]
    async fn pulled_artifacts_live_until_their_owning_images_are_dropped() {
        let platform = platform::HostPlatform::current().expect("test host should be supported");
        let registry = RegistryFixture::start(RegistryDocuments::new(
            &platform,
            InitialDocument::Index,
            Corruption::None,
        ))
        .await;
        let fetcher = OciFetcher::for_test(platform);
        let (first_status, first, first_image) =
            request_pull(&fetcher, &registry.manifest_url, TOKEN).await;
        let (second_status, second, second_image) =
            request_pull(&fetcher, &registry.manifest_url, TOKEN).await;
        assert_eq!(first_status, StatusCode::OK);
        assert_eq!(second_status, StatusCode::OK);
        let first_path = Path::new(
            first["filesystem"]["path"]
                .as_str()
                .expect("first filesystem path should be a string"),
        )
        .to_path_buf();
        let second_path = Path::new(
            second["filesystem"]["path"]
                .as_str()
                .expect("second filesystem path should be a string"),
        )
        .to_path_buf();
        assert_ne!(first_path, second_path);
        assert!(first_path.is_file());
        assert!(second_path.is_file());
        drop(first_image);
        assert!(!first_path.exists());
        assert!(second_path.exists());
        drop(second_image);
        assert!(!first_path.exists());
        assert!(!second_path.exists());
    }
}
