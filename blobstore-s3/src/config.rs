//! Configuration for blobstore-s3 capability provider
//!
//! The simplest and preferred way to set configuration is to set environment variables
//! - `AWS_ACCESS_KEY_ID`
//! - `AWS_SECRET_ACCESS_KEY`
//! - and, optionally, `AWS_SESSION_TOKEN`
//! If STS AssumeRole is to be used, the same environment variables
//! will be used to create in initial environment, and then the
//! settings fom `sts_config` will be used to create a session for the assumed role
//!
use aws_types::{
    config::Config as AwsConfig, credentials::SharedCredentialsProvider, region::Region,
};
use serde::Deserialize;
use std::{collections::HashMap, env};
use wasmbus_rpc::error::{RpcError, RpcResult};

const DEFAULT_STS_SESSION: &str = "blobstore_s3_provider";

/// Configuration for connecting to S3.
///
#[derive(Clone, Default, Deserialize)]
pub struct StorageConfig {
    /// AWS_ACCESS_KEY_ID, can be specified from environment
    pub access_key_id: Option<String>,
    /// AWS_SECRET_ACCESS_KEY, can be in environment
    pub secret_access_key: Option<String>,
    /// Session Token
    pub session_token: Option<String>,
    /// AWS_REGION
    pub region: Option<String>,
    /// override default max_attempts (3) for retries
    pub max_attempts: Option<u32>,

    pub sts_config: Option<StsAssumeRoleConfig>,
}

#[derive(Clone, Default, Deserialize)]
pub struct StsAssumeRoleConfig {
    /// Role to assume (AWS_ASSUME_ROLE_ARN)
    /// Should be in the form "arn:aws:iam::123456789012:role/example"
    pub role: String,
    /// AWS Region for using sts, not for S3
    pub region: Option<String>,
    /// Optional Session name
    pub session: Option<String>,
    /// Optional external id
    pub external_id: Option<String>,
}

impl StorageConfig {
    /// initialize from linkdef values
    pub fn from_values(values: &HashMap<String, String>) -> RpcResult<StorageConfig> {
        let mut config = if let Some(config_b64) = values.get("config_b64") {
            let bytes = base64::decode(config_b64.as_bytes()).map_err(|e| {
                RpcError::InvalidParameter(format!("invalid base64 encoding: {}", e))
            })?;
            serde_json::from_slice::<StorageConfig>(&bytes)
                .map_err(|e| RpcError::InvalidParameter(format!("corrupt config_b64: {}", e)))?
        } else if let Some(config) = values.get("config_json") {
            serde_json::from_str::<StorageConfig>(config)
                .map_err(|e| RpcError::InvalidParameter(format!("corrupt config_json: {}", e)))?
        } else {
            StorageConfig::default()
        };
        if let Ok(arn) = env::var("AWS_ASSUME_ROLE_ARN") {
            let mut sts_config = config.sts_config.unwrap_or_default();
            sts_config.role = arn;
            if let Ok(region) = env::var("AWS_ASSUME_ROLE_REGION") {
                sts_config.region = Some(region);
            }
            if let Ok(session) = env::var("AWS_ASSUME_ROLE_SESSION") {
                sts_config.session = Some(session);
            }
            if let Ok(external_id) = env::var("AWS_ASSUME_ROLE_EXTERNAL_ID") {
                sts_config.external_id = Some(external_id);
            }
            config.sts_config = Some(sts_config);
        }
        Ok(config)
    }

    pub async fn configure_aws(self) -> AwsConfig {
        use aws_config::{
            default_provider::{credentials::DefaultCredentialsChain, region::DefaultRegionChain},
            sts::AssumeRoleProvider,
        };

        let region = match self.region {
            Some(region) => Some(Region::new(region)),
            _ => DefaultRegionChain::builder().build().region().await,
        };

        // use static credentials or defaults from environment
        let mut cred_provider = match (self.access_key_id, self.secret_access_key) {
            (Some(access_key_id), Some(secret_access_key)) => {
                SharedCredentialsProvider::new(aws_types::credentials::Credentials::from_keys(
                    access_key_id,
                    secret_access_key,
                    self.session_token.clone(),
                ))
            }
            _ => SharedCredentialsProvider::new(
                DefaultCredentialsChain::builder()
                    .region(region.clone())
                    .build()
                    .await,
            ),
        };

        if let Some(sts_config) = self.sts_config {
            let mut role = AssumeRoleProvider::builder(sts_config.role).session_name(
                sts_config
                    .session
                    .unwrap_or_else(|| DEFAULT_STS_SESSION.to_string()),
            );
            if let Some(region) = sts_config.region {
                role = role.region(Region::new(region));
            }
            if let Some(external_id) = sts_config.external_id {
                role = role.external_id(external_id);
            }
            cred_provider = SharedCredentialsProvider::new(role.build(cred_provider));
        }

        let mut retry_config = aws_config::RetryConfig::new();
        if let Some(max_attempts) = self.max_attempts {
            retry_config = retry_config.with_max_attempts(max_attempts);
        }
        let loader = aws_config::from_env()
            .region(region)
            .credentials_provider(cred_provider)
            .retry_config(retry_config);
        loader.load().await
    }
}
