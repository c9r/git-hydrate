//! An S3 bucket as the object store, through any S3 API.
//!
//! Objects are uploaded as multipart uploads with parallel parts and read as
//! ranged requests. Integrity is the tool's own sha256 over the bytes in
//! order, so the SDK's checksum headers are turned off, which keeps storage
//! that never implemented them working.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use aws_config::{BehaviorVersion, Region, retry::RetryConfig};
use aws_sdk_s3::Client;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_smithy_types::checksum_config::{RequestChecksumCalculation, ResponseChecksumValidation};
use aws_smithy_types::error::display::DisplayErrorContext;
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use indicatif::ProgressBar;

use crate::config::Settings;
use crate::hash::Hasher;

/// S3 accepts at most this many parts in one multipart upload.
const MAX_PARTS: u64 = 10_000;

/// How many times a request is attempted before its error is reported.
const ATTEMPTS: u32 = 5;

pub struct S3Remote {
    client: Client,
    bucket: String,
    prefix: String,
    /// Bytes per multipart upload part.
    partsize: u64,
    /// Parts in flight per upload.
    concurrency: usize,
}

impl S3Remote {
    pub async fn open(settings: &Settings, bucket: &str, prefix: &str) -> Result<S3Remote> {
        let mut loader = aws_config::defaults(BehaviorVersion::latest())
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
            .retry_config(RetryConfig::standard().with_max_attempts(ATTEMPTS));
        if let Some(profile) = &settings.profile {
            loader = loader.profile_name(profile);
        }
        if let Some(region) = &settings.region {
            loader = loader.region(Region::new(region.clone()));
        }
        if let Some(endpoint) = &settings.endpoint {
            loader = loader.endpoint_url(endpoint);
        }
        let sdk = loader.load().await;
        if sdk.region().is_none() {
            bail!("no S3 region is configured; set hydrate.region or a region in the AWS profile");
        }
        let mut builder = aws_sdk_s3::config::Builder::from(&sdk);
        if settings.pathstyle {
            builder = builder.force_path_style(true);
        }
        Ok(S3Remote {
            client: Client::from_conf(builder.build()),
            bucket: bucket.to_string(),
            prefix: prefix.to_string(),
            partsize: settings.partsize,
            concurrency: settings.concurrency,
        })
    }

    pub fn describe(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.prefix)
    }

    fn key(&self, oid: &str) -> String {
        format!("{}{oid}", self.prefix)
    }

    pub async fn head(&self, oid: &str) -> Result<Option<u64>> {
        let result = self.client.head_object().bucket(&self.bucket).key(self.key(oid)).send().await;
        match result {
            Ok(output) => Ok(Some(output.content_length().unwrap_or(0) as u64)),
            Err(err) => {
                let not_found = err.as_service_error().map(|e| e.is_not_found()).unwrap_or(false)
                    || err.raw_response().map(|r| r.status().as_u16() == 404).unwrap_or(false);
                if not_found { Ok(None) } else { Err(describe(err)).with_context(|| format!("checking {oid}")) }
            }
        }
    }

    pub async fn get_range(&self, oid: &str, start: u64, len: u64) -> Result<Bytes> {
        let end = start + len - 1;
        let output = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.key(oid))
            .range(format!("bytes={start}-{end}"))
            .send()
            .await
            .map_err(describe)
            .with_context(|| format!("reading {oid} at {start}"))?;
        let bytes = output.body.collect().await.with_context(|| format!("reading {oid} at {start}"))?.into_bytes();
        if bytes.len() as u64 != len {
            bail!("the remote returned {} bytes of {oid} at {start} instead of {len}", bytes.len());
        }
        Ok(bytes)
    }

    pub async fn put_file(&self, oid: &str, size: u64, path: &Path, bar: &ProgressBar) -> Result<()> {
        if let Some(existing) = self.head(oid).await? {
            if existing != size {
                bail!("the remote already holds {oid} at {existing} bytes, not {size}");
            }
            bar.inc(size);
            return Ok(());
        }
        if size <= self.partsize {
            return self.put_whole(oid, size, path, bar).await;
        }
        let partsize = self.partsize.max(size.div_ceil(MAX_PARTS));
        let upload_id = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(self.key(oid))
            .send()
            .await
            .map_err(describe)
            .with_context(|| format!("starting the upload of {oid}"))?
            .upload_id()
            .context("the remote returned no upload id")?
            .to_string();
        match self.put_parts(oid, size, path, partsize, &upload_id, bar).await {
            Ok(parts) => {
                self.client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(self.key(oid))
                    .upload_id(&upload_id)
                    .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build())
                    .send()
                    .await
                    .map_err(describe)
                    .with_context(|| format!("completing the upload of {oid}"))?;
                Ok(())
            }
            Err(err) => {
                let _ = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(self.key(oid))
                    .upload_id(&upload_id)
                    .send()
                    .await;
                Err(err)
            }
        }
    }

    async fn put_whole(&self, oid: &str, size: u64, path: &Path, bar: &ProgressBar) -> Result<()> {
        let bytes = tokio::fs::read(path).await.with_context(|| format!("reading {}", path.display()))?;
        let mut hasher = Hasher::new();
        hasher.update(&bytes);
        let actual = hasher.finish();
        if actual != oid || bytes.len() as u64 != size {
            bail!("{} hashes to {actual} at {} bytes, not {oid} at {size}", path.display(), bytes.len());
        }
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(self.key(oid))
            .content_length(size as i64)
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map_err(describe)
            .with_context(|| format!("uploading {oid}"))?;
        bar.inc(size);
        Ok(())
    }

    /// Uploads the parts of a file, reading and hashing it in order on one
    /// thread while up to `concurrency` parts are in flight.
    async fn put_parts(
        &self,
        oid: &str,
        size: u64,
        path: &Path,
        partsize: u64,
        upload_id: &str,
        bar: &ProgressBar,
    ) -> Result<Vec<CompletedPart>> {
        let (sender, receiver) = tokio::sync::mpsc::channel::<(i32, Bytes)>(self.concurrency);
        let source = path.to_path_buf();
        let reader = tokio::task::spawn_blocking(move || -> Result<(String, u64)> {
            let mut file = File::open(&source).with_context(|| format!("opening {}", source.display()))?;
            let mut hasher = Hasher::new();
            let mut total = 0u64;
            let mut part_number = 1i32;
            loop {
                let mut buf = vec![0u8; partsize as usize];
                let n = read_full(&mut file, &mut buf).with_context(|| format!("reading {}", source.display()))?;
                if n == 0 {
                    break;
                }
                buf.truncate(n);
                hasher.update(&buf);
                total += n as u64;
                if sender.blocking_send((part_number, Bytes::from(buf))).is_err() {
                    break;
                }
                part_number += 1;
            }
            Ok((hasher.finish(), total))
        });

        let parts = futures::stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|part| (part, receiver))
        })
        .map(|(part_number, bytes)| {
            let client = self.client.clone();
            let bucket = self.bucket.clone();
            let key = self.key(oid);
            let upload_id = upload_id.to_string();
            let bar = bar.clone();
            async move {
                let len = bytes.len() as u64;
                let output = client
                    .upload_part()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .part_number(part_number)
                    .content_length(len as i64)
                    .body(ByteStream::from(bytes))
                    .send()
                    .await
                    .map_err(describe)
                    .with_context(|| format!("uploading part {part_number}"))?;
                bar.inc(len);
                let e_tag = output.e_tag().context("the remote returned no etag for a part")?.to_string();
                Ok::<CompletedPart, anyhow::Error>(
                    CompletedPart::builder().part_number(part_number).e_tag(e_tag).build(),
                )
            }
        })
        .buffer_unordered(self.concurrency)
        .try_collect::<Vec<CompletedPart>>()
        .await;

        let (actual, total) = reader.await.context("the read task failed")??;
        let mut parts = parts?;
        if actual != oid || total != size {
            bail!("{} hashes to {actual} at {total} bytes, not {oid} at {size}", path.display());
        }
        parts.sort_by_key(|part| part.part_number());
        Ok(parts)
    }
}

/// Reads until the buffer is full or the file ends, returning the count read.
fn read_full(file: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Turns an SDK error into one whose message carries the whole cause chain.
fn describe<E, R>(err: SdkError<E, R>) -> anyhow::Error
where
    E: std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug + Send + Sync + 'static,
{
    anyhow!("{}", DisplayErrorContext(&err))
}
