use aws_smithy_types::retry::{RetryConfig, RetryMode};
use cargo_lambda_build::{find_binary_archive, zip_binary, BinaryArchive};
use cargo_lambda_interactive::progress::Progress;
use cargo_lambda_metadata::cargo::main_binary;
use cargo_lambda_remote::{
    aws_sdk_lambda::model::{Architecture, Runtime},
    RemoteConfig,
};
use clap::{Args, ValueHint};
use miette::{IntoDiagnostic, Result, WrapErr};
use serde::Serialize;
use serde_json::ser::to_string_pretty;
use std::{collections::HashMap, fs::read, path::PathBuf, time::Duration};
use strum_macros::{Display, EnumString};

mod extensions;
mod functions;
mod roles;

#[derive(Clone, Debug, Display, EnumString)]
#[strum(ascii_case_insensitive)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Serialize)]
#[serde(untagged)]
enum DeployResult {
    Extension(extensions::DeployOutput),
    Function(functions::DeployOutput),
}

impl std::fmt::Display for DeployResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeployResult::Extension(o) => o.fmt(f),
            DeployResult::Function(o) => o.fmt(f),
        }
    }
}

#[derive(Args, Clone, Debug)]
#[command(
    name = "deploy",
    after_help = "Full command documentation: https://www.cargo-lambda.info/commands/deploy.html"
)]
pub struct Deploy {
    #[command(flatten)]
    remote_config: RemoteConfig,

    #[command(flatten)]
    function_config: functions::FunctionDeployConfig,

    /// Directory where the lambda binaries are located
    #[arg(short, long, value_hint = ValueHint::DirPath)]
    lambda_dir: Option<PathBuf>,

    /// Path to Cargo.toml
    #[arg(long, value_name = "PATH", default_value = "Cargo.toml")]
    pub manifest_path: PathBuf,

    /// Name of the binary to deploy if it doesn't match the name that you want to deploy it with
    #[arg(long)]
    pub binary_name: Option<String>,

    /// Local path of the binary to deploy if it doesn't match the target path generated by cargo-lambda-build
    #[arg(long)]
    pub binary_path: Option<PathBuf>,

    /// S3 bucket to upload the code to
    #[arg(long)]
    pub s3_bucket: Option<String>,

    /// Whether the code that you're deploying is a Lambda Extension
    #[arg(long)]
    extension: bool,

    /// Whether an extension is internal or external
    #[arg(long, requires = "extension")]
    internal: bool,

    /// Comma separated list with compatible runtimes for the Lambda Extension (--compatible_runtimes=provided.al2,nodejs16.x)
    /// List of allowed runtimes can be found in the AWS documentation: https://docs.aws.amazon.com/lambda/latest/dg/API_CreateFunction.html#SSS-CreateFunction-request-Runtime
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "provided.al2",
        requires = "extension"
    )]
    compatible_runtimes: Vec<String>,

    /// Format to render the output (text, or json)
    #[arg(short, long, default_value_t = OutputFormat::Text)]
    output_format: OutputFormat,

    /// Option to add one or many tags, allows multiple repetitions (--tag organization=aws --tag team=lambda)
    /// This option overrides any values set with the --tags flag.
    #[arg(long)]
    tag: Option<Vec<String>>,

    /// Comma separated list of tags to apply to the function or extension (--tags organization=aws,team=lambda)
    /// This option overrides any values set with the --tag flag.
    #[arg(long, value_delimiter = ',')]
    tags: Option<Vec<String>>,

    /// Option to add one or more files and directories to include in the zip file to upload.
    #[arg(short, long)]
    include: Option<Vec<PathBuf>>,

    /// Name of the function or extension to deploy
    #[arg(value_name = "NAME")]
    name: Option<String>,
}

impl Deploy {
    #[tracing::instrument(skip(self), target = "cargo_lambda")]
    pub async fn run(&self) -> Result<()> {
        tracing::trace!(options = ?self, "deploying project");

        if self.function_config.enable_function_url && self.function_config.disable_function_url {
            return Err(miette::miette!("invalid options: --enable-function-url and --disable-function-url cannot be set together"));
        }

        let progress = Progress::start("loading binary data");
        let (name, archive) = match self.load_archive() {
            Ok(arc) => arc,
            Err(err) => {
                progress.finish_and_clear();
                return Err(err);
            }
        };

        let retry = RetryConfig::standard()
            .with_retry_mode(RetryMode::Adaptive)
            .with_max_attempts(3)
            .with_initial_backoff(Duration::from_secs(5));

        let sdk_config = self.remote_config.sdk_config(Some(retry)).await;
        let architecture = Architecture::from(archive.architecture.as_str());
        let compatible_runtimes = self
            .compatible_runtimes
            .iter()
            .map(|runtime| Runtime::from(runtime.as_str()))
            .collect::<Vec<_>>();

        let binary_data = read(&archive.path)
            .into_diagnostic()
            .wrap_err("failed to read binary archive")?;

        let mut tags = self.tags.clone();
        if tags.is_none() {
            tags = self.tag.clone();
        }

        let result = if self.extension {
            extensions::deploy(
                &name,
                &self.manifest_path,
                &sdk_config,
                binary_data,
                architecture,
                compatible_runtimes,
                &self.s3_bucket,
                &tags,
                &progress,
            )
            .await
        } else {
            let binary_name = self.binary_name.clone().unwrap_or_else(|| name.clone());
            functions::deploy(
                &name,
                &binary_name,
                &self.manifest_path,
                &self.function_config,
                &self.remote_config,
                &sdk_config,
                &self.s3_bucket,
                &tags,
                binary_data,
                architecture,
                &progress,
            )
            .await
        };

        progress.finish_and_clear();
        let output = result?;

        match &self.output_format {
            OutputFormat::Text => println!("{output}"),
            OutputFormat::Json => {
                let text = to_string_pretty(&output)
                    .into_diagnostic()
                    .wrap_err("failed to serialize output into json")?;
                println!("{text}")
            }
        }

        Ok(())
    }

    fn load_archive(&self) -> Result<(String, BinaryArchive)> {
        let arc = match &self.binary_path {
            Some(bp) if bp.is_dir() => return Err(miette::miette!("invalid file {:?}", bp)),
            Some(bp) => {
                let name = match &self.name {
                    Some(name) => name.clone(),
                    None => bp
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(String::from)
                        .ok_or_else(|| miette::miette!("invalid binary path {:?}", bp))?,
                };

                let destination = bp
                    .parent()
                    .ok_or_else(|| miette::miette!("invalid binary path {:?}", bp))?;

                let parent = if self.extension && !self.internal {
                    Some("extensions")
                } else {
                    None
                };

                let arc = zip_binary(&name, bp, destination, parent, self.include.clone())?;
                (name, arc)
            }
            None => {
                let name = match &self.name {
                    Some(name) => name.clone(),
                    None => main_binary(&self.manifest_path).into_diagnostic()?,
                };
                let binary_name = self.binary_name.as_deref().unwrap_or(&name);

                let arc = find_binary_archive(
                    binary_name,
                    &self.manifest_path,
                    &self.lambda_dir,
                    self.extension,
                    self.internal,
                    self.include.clone(),
                )?;
                (name, arc)
            }
        };
        Ok(arc)
    }
}

pub(crate) fn extract_tags(tags: &Vec<String>) -> HashMap<String, String> {
    let mut map = HashMap::new();

    for var in tags {
        let mut split = var.splitn(2, '=');
        if let (Some(k), Some(v)) = (split.next(), split.next()) {
            map.insert(k.to_string(), v.to_string());
        }
    }

    map
}
