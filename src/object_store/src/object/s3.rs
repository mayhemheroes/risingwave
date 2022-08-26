// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use aws_sdk_s3::model::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::output::UploadPartOutput;
use aws_sdk_s3::{Client, Endpoint, Region};
use fail::fail_point;
use futures::future::try_join_all;
use futures::stream;
use hyper::Body;
use itertools::Itertools;
use tokio::task::JoinHandle;

use super::object_metrics::ObjectStoreMetrics;
use super::{
    BlockLocation, BoxedStreamingUploader, Bytes, ObjectError, ObjectMetadata, ObjectResult,
    ObjectStore, StreamingUploader,
};

type PartId = i32;

/// MinIO and S3 share the same minimum part ID and part size.
const MIN_PART_ID: PartId = 1;
const MIN_PART_SIZE: usize = 5 * 1024 * 1024;

const S3_PART_SIZE: usize = 16 * 1024 * 1024;
// TODO: we should do some benchmark to determine the proper part size for MinIO
const MINIO_PART_SIZE: usize = 16 * 1024 * 1024;

/// S3 multipart upload handle.
/// Reference: <https://docs.aws.amazon.com/AmazonS3/latest/userguide/mpuoverview.html>
pub struct S3StreamingUploader {
    client: Client,
    part_size: usize,
    bucket: String,
    /// The key of the object.
    key: String,
    /// The identifier of multipart upload task for S3.
    upload_id: String,
    /// Next part ID.
    next_part_id: PartId,
    /// Join handles for part uploads.
    join_handles: Vec<JoinHandle<ObjectResult<(PartId, UploadPartOutput)>>>,
    /// Buffer for bytes.
    ///
    /// We prefer `Vec` over other data structures for better memory usage
    /// and spatial locality, which is important because we tend to remove multiple
    /// consecutive elements from `buf` at a time.
    ///
    /// Moreover, we preserve at least `MIN_PART_SIZE` of data in the buffer when uploading a part
    /// due to the minimum part size limitation of S3.
    buf: Vec<Bytes>,
    /// Length of the data that have not been uploaded to S3.
    not_uploaded_len: usize,
    /// Length of the data that exceeds the current part to be uploaded in the buffer.
    next_part_len: usize,
    /// The data included in the next part are `buf[..part_end]`.
    part_end: usize,
    /// To record metrics for uploading part.
    metrics: Arc<ObjectStoreMetrics>,
}

impl S3StreamingUploader {
    pub fn new(
        client: Client,
        bucket: String,
        part_size: usize,
        key: String,
        upload_id: String,
        metrics: Arc<ObjectStoreMetrics>,
    ) -> S3StreamingUploader {
        Self {
            client,
            bucket,
            part_size,
            key,
            upload_id,
            next_part_id: MIN_PART_ID,
            join_handles: Default::default(),
            buf: Default::default(),
            not_uploaded_len: 0,
            next_part_len: 0,
            part_end: 0,
            metrics,
        }
    }

    fn upload_next_part(&mut self, data: Vec<Bytes>, len: usize) {
        debug_assert_eq!(data.iter().map(Bytes::len).sum::<usize>(), len);

        let part_id = self.next_part_id;
        self.next_part_id += 1;
        let client_cloned = self.client.clone();
        let bucket = self.bucket.clone();
        let key = self.key.clone();
        let upload_id = self.upload_id.clone();
        let metrics = self.metrics.clone();
        metrics
            .operation_size
            .with_label_values(&["s3_upload_part"])
            .observe(len as f64);

        self.join_handles.push(tokio::spawn(async move {
            let timer = metrics
                .operation_latency
                .with_label_values(&["s3", "s3_upload_part"])
                .start_timer();
            let upload_output = client_cloned
                .upload_part()
                .bucket(bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(part_id)
                .body(get_upload_body(data))
                .content_length(len as i64)
                .send()
                .await?;
            timer.observe_duration();
            Ok((part_id, upload_output))
        }));
    }

    async fn flush_and_complete(&mut self) -> ObjectResult<()> {
        self.upload_next_part(
            Vec::from_iter(self.buf.iter().cloned()),
            self.not_uploaded_len,
        );

        // If any part fails to upload, abort the upload.
        let join_handles = self.join_handles.drain(..).collect_vec();

        let mut uploaded_parts = Vec::with_capacity(join_handles.len());
        for result in try_join_all(join_handles)
            .await
            .map_err(ObjectError::internal)?
        {
            uploaded_parts.push(result?);
        }

        let completed_parts = Some(
            uploaded_parts
                .iter()
                .map(|(part_id, output)| {
                    CompletedPart::builder()
                        .set_e_tag(output.e_tag.clone())
                        .set_part_number(Some(*part_id))
                        .build()
                })
                .collect_vec(),
        );

        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(&self.key)
            .upload_id(&self.upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(completed_parts)
                    .build(),
            )
            .send()
            .await?;

        Ok(())
    }

    async fn abort(&self) -> ObjectResult<()> {
        // If any part uploads are currently in progress, those part uploads might or might
        // not succeed. As a result, it might be necessary to abort a given multipart upload
        // multiple times in order to completely free all storage consumed by all parts.
        //
        // To verify that all parts have been removed, so you don't get charged for the
        // part storage, you should call the ListParts action and ensure that the parts list is
        // empty.
        //
        // Reference: <https://docs.aws.amazon.com/AmazonS3/latest/API/API_AbortMultipartUpload.html>
        self.client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(&self.key)
            .upload_id(&self.upload_id)
            .send()
            .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl StreamingUploader for S3StreamingUploader {
    fn write_bytes(&mut self, data: Bytes) -> ObjectResult<()> {
        fail_point!("s3_write_bytes_err", |_| Err(ObjectError::internal(
            "s3 write bytes error"
        )));
        let data_len = data.len();
        self.not_uploaded_len += data_len;
        self.buf.push(data);

        if self.not_uploaded_len > self.part_size {
            if self.part_end == 0 {
                // Mark current slice of buffer to be the next part to be uploaded.
                self.part_end = self.buf.len();
            } else {
                // `data` should be uploaded in the next part.
                self.next_part_len += data_len;
            }
        }
        if self.next_part_len >= MIN_PART_SIZE {
            // Take a 16MiB part and upload it. `Bytes` performs shallow clone.
            let part = self.buf.drain(..self.part_end).collect();
            self.upload_next_part(part, self.not_uploaded_len - self.next_part_len);
            self.not_uploaded_len = self.next_part_len;
            self.part_end = 0;
            self.next_part_len = 0;
        }
        Ok(())
    }

    /// If the data in the buffer is smaller than `MIN_PART_SIZE`, abort multipart upload
    /// and use `PUT` to upload the data. Otherwise flush the remaining data of the buffer
    /// to S3 as a new part. Fallback to `PUT` on failure.
    async fn finish(mut self: Box<Self>) -> ObjectResult<()> {
        fail_point!("s3_finish_streaming_upload_err", |_| Err(
            ObjectError::internal("s3 finish streaming upload error")
        ));
        // Fallback to `PUT`.
        if self.join_handles.is_empty() {
            self.abort().await?;
            return if self.buf.is_empty() {
                Err(ObjectError::internal("upload empty object"))
            } else {
                self.client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(&self.key)
                    .body(get_upload_body(self.buf))
                    .content_length(self.not_uploaded_len as i64)
                    .send()
                    .await?;
                Ok(())
            };
        }
        if let Err(e) = self.flush_and_complete().await {
            self.abort().await?;
            return Err(e);
        }
        Ok(())
    }
}

fn get_upload_body(data: Vec<Bytes>) -> aws_sdk_s3::types::ByteStream {
    Body::wrap_stream(stream::iter(data.into_iter().map(ObjectResult::Ok))).into()
}

/// Object store with S3 backend
pub struct S3ObjectStore {
    client: Client,
    bucket: String,
    part_size: usize,
    /// For S3 specific metrics.
    metrics: Arc<ObjectStoreMetrics>,
}

#[async_trait::async_trait]
impl ObjectStore for S3ObjectStore {
    async fn upload(&self, path: &str, obj: Bytes) -> ObjectResult<()> {
        fail_point!("s3_upload_err", |_| Err(ObjectError::internal(
            "s3 upload error"
        )));
        if obj.is_empty() {
            Err(ObjectError::internal("upload empty object"))
        } else {
            self.client
                .put_object()
                .bucket(&self.bucket)
                .body(aws_sdk_s3::types::ByteStream::from(obj))
                .key(path)
                .send()
                .await?;
            Ok(())
        }
    }

    async fn streaming_upload(&self, path: &str) -> ObjectResult<BoxedStreamingUploader> {
        fail_point!("s3_streaming_upload_err", |_| Err(ObjectError::internal(
            "s3 streaming upload error"
        )));
        let resp = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(path)
            .send()
            .await?;
        Ok(Box::new(S3StreamingUploader::new(
            self.client.clone(),
            self.bucket.clone(),
            self.part_size,
            path.to_string(),
            resp.upload_id.unwrap(),
            self.metrics.clone(),
        )))
    }

    /// Amazon S3 doesn't support retrieving multiple ranges of data per GET request.
    async fn read(&self, path: &str, block_loc: Option<BlockLocation>) -> ObjectResult<Bytes> {
        fail_point!("s3_read_err", |_| Err(ObjectError::internal(
            "s3 read error"
        )));
        let req = self.client.get_object().bucket(&self.bucket).key(path);

        let range = match block_loc.as_ref() {
            None => None,
            Some(block_location) => block_location.byte_range_specifier(),
        };

        let req = if let Some(range) = range {
            req.range(range)
        } else {
            req
        };

        let resp = req.send().await?;
        let val = resp.body.collect().await?.into_bytes();

        if block_loc.is_some() && block_loc.as_ref().unwrap().size != val.len() {
            return Err(ObjectError::internal(format!(
                "mismatched size: expected {}, found {} when reading {} at {:?}",
                block_loc.as_ref().unwrap().size,
                val.len(),
                path,
                block_loc.as_ref().unwrap()
            )));
        }
        Ok(val)
    }

    async fn readv(&self, path: &str, block_locs: &[BlockLocation]) -> ObjectResult<Vec<Bytes>> {
        let futures = block_locs
            .iter()
            .map(|block_loc| self.read(path, Some(*block_loc)))
            .collect_vec();
        try_join_all(futures).await
    }

    async fn metadata(&self, path: &str) -> ObjectResult<ObjectMetadata> {
        fail_point!("s3_metadata_err", |_| Err(ObjectError::internal(
            "s3 metadata error"
        )));
        let resp = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(path)
            .send()
            .await?;
        Ok(ObjectMetadata {
            key: path.to_owned(),
            last_modified: resp
                .last_modified()
                .expect("last_modified required")
                .as_secs_f64(),
            total_size: resp.content_length as usize,
        })
    }

    /// Permanently deletes the whole object.
    /// According to Amazon S3, this will simply return Ok if the object does not exist.
    async fn delete(&self, path: &str) -> ObjectResult<()> {
        fail_point!("s3_delete_err", |_| Err(ObjectError::internal(
            "s3 delete error"
        )));
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(path)
            .send()
            .await?;
        Ok(())
    }

    async fn list(&self, prefix: &str) -> ObjectResult<Vec<ObjectMetadata>> {
        let mut ret: Vec<ObjectMetadata> = vec![];
        let mut next_continuation_token = None;
        // list_objects_v2 returns up to 1000 keys and truncated the exceeded parts.
        // Use `continuation_token` given by last response to fetch more parts of the result,
        // until result is no longer truncated.
        loop {
            let mut request = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);
            if let Some(continuation_token) = next_continuation_token.take() {
                request = request.continuation_token(continuation_token);
            }
            let result = request.send().await?;
            let is_truncated = result.is_truncated;
            ret.append(
                &mut result
                    .contents()
                    .unwrap_or_default()
                    .iter()
                    .map(|obj| ObjectMetadata {
                        key: obj.key().expect("key required").to_owned(),
                        last_modified: obj
                            .last_modified()
                            .expect("last_modified required")
                            .as_secs_f64(),
                        total_size: obj.size() as usize,
                    })
                    .collect_vec(),
            );
            next_continuation_token = result.next_continuation_token;
            if !is_truncated {
                break;
            }
        }
        Ok(ret)
    }

    fn store_media_type(&self) -> &'static str {
        "s3"
    }
}

impl S3ObjectStore {
    /// Creates an S3 object store from environment variable.
    ///
    /// See [AWS Docs](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/credentials.html) on how to provide credentials and region from env variable. If you are running compute-node on EC2, no configuration is required.
    pub async fn new(bucket: String, metrics: Arc<ObjectStoreMetrics>) -> Self {
        let shared_config = aws_config::load_from_env().await;
        let client = Client::new(&shared_config);

        Self {
            client,
            bucket,
            part_size: S3_PART_SIZE,
            metrics,
        }
    }

    /// Creates a minio client. The server should be like `minio://key:secret@address:port/bucket`.
    pub async fn with_minio(server: &str, metrics: Arc<ObjectStoreMetrics>) -> Self {
        let server = server.strip_prefix("minio://").unwrap();
        let (access_key_id, rest) = server.split_once(':').unwrap();
        let (secret_access_key, rest) = rest.split_once('@').unwrap();
        let (address, bucket) = rest.split_once('/').unwrap();

        let loader = aws_config::ConfigLoader::default();
        let builder = aws_sdk_s3::config::Builder::from(&loader.load().await)
            .region(Region::new("custom"))
            .endpoint_resolver(Endpoint::immutable(
                format!("http://{}", address).try_into().unwrap(),
            ))
            .credentials_provider(aws_sdk_s3::Credentials::from_keys(
                access_key_id,
                secret_access_key,
                None,
            ));
        let config = builder.build();
        let client = Client::from_conf(config);
        Self {
            client,
            bucket: bucket.to_string(),
            part_size: MINIO_PART_SIZE,
            metrics,
        }
    }
}
