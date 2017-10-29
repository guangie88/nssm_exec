#[macro_use]
extern crate derive_error_chain;
#[macro_use]
extern crate error_chain;
extern crate file;
#[macro_use]
extern crate log;
extern crate log4rs;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate simple_logger;
extern crate structopt;
#[macro_use]
extern crate structopt_derive;
extern crate toml;

use std::fmt::Display;
use std::thread;
use std::time::Duration;
use std::path::PathBuf;
use std::process::{self, Command, Output};
use structopt::StructOpt;

struct OtherConfigRef<'a, 'b, 'c> {
    deps: Option<&'a String>,
    start_on_create: Option<&'b bool>,
    account: Option<&'c Account>,
}

/// Groups the Windows account settings for running a service.
#[derive(Deserialize)]
struct Account {
    /// Windows account username.
    user: String,

    /// Password corresponding to the username.
    /// May be left as empty string if username does not require password.
    password: String,
}

/// Groups the extra configurations required for configuring the service.
/// May be used on every service or in a global context.
#[derive(Deserialize)]
struct OtherConfig {
    /// List of other service names to depend on before starting this service.
    /// Multiple service names are space delimited.
    deps: Option<String>,

    /// States whether to immediately start the created service.
    /// Defaults to false.
    start_on_create: Option<bool>,

    /// Holds the account configuration to run the service.
    account: Option<Account>,
}

/// Groups the configurations required for a service.
#[derive(Deserialize)]
struct Service {
    /// Name of service.
    name: String,

    /// Service executable file path.
    path: PathBuf,

    /// Service startup directory path. Leaving empty should use the directory path
    /// containing the executable.
    startup_dir: Option<PathBuf>,

    /// Arguments to be passed into the executable. Multiple arguments are space delimited and
    /// arguments may be wrapped around double quotes like in cmd.
    args: Option<String>,

    /// Description string of service.
    description: Option<String>,

    /// Holds the extra configurations.
    /// Any specific extra configurations will always override the global ones.
    other: Option<OtherConfig>,
}

/// Represents the TOML nssm_exec configuration.
#[derive(Deserialize)]
struct FileConfig {
    /// NSSM executable file path
    nssm_path: PathBuf,

    /// Interval in milliseconds before retrying to check if the service has stopped.
    /// Default is 500. Only applicable if there is any running existing service.
    pending_stop_poll_ms: Option<u64>,

    /// Number of retries to check if the service has stopped.
    /// Default is 5. Only applicable if there is any running existing service.
    pending_stop_poll_count: Option<u64>,

    /// Holds the global extra configurations.
    /// Any specific extra configurations will always override the global ones.
    global: Option<OtherConfig>,

    /// Holds the service configurations.
    services: Vec<Service>,
}

#[derive(StructOpt, Debug)]
#[structopt(name = "NSSM Executor", about = "Program to facilitate easy adding of nssm services.")]
struct MainConfig {
    #[structopt(short = "c", long = "conf", help = "TOML configuration to set up nssm",
                default_value = "config/nssm_exec.toml")]
    config_path: String,

    #[structopt(short = "l", long = "log", help = "Logging configuration file path",
                default_value = "config/logging_nssm_exec.yml")]
    log_config_path: Option<String>,
}

mod errors {
    #[derive(Debug, ErrorChain)]
    pub enum ErrorKind {
        Msg(String),
    }
}

use errors::*;

const SERVICE_STOP_PENDING_STATUS: &str = "SERVICE_STOP_PENDING";
const SERVICE_STOPPED_STATUS: &str = "SERVICE_STOPPED";
const PENDING_STOP_POLL_MS_DEF: u64 = 500;
const PENDING_STOP_POLL_COUNT_DEF: u64 = 5;

trait ChainService<T> {
    fn chain_service_msg(self, description: &str, service_name: &str) -> Result<T>;
}

impl<T, E> ChainService<T> for std::result::Result<T, E>
where
    E: std::error::Error + Send + 'static,
{
    fn chain_service_msg(self, description: &str, service_name: &str) -> Result<T> {
        self.chain_err(|| format!("{} service '{}'", description, service_name))
    }
}

fn run_cmd(cmd: &str) -> Result<Output> {
    debug!("{}", cmd);

    let output = if cfg!(target_os = "windows") {
        Command::new("cmd").args(&["/C", &cmd]).output()
    } else {
        Command::new("sh").args(&["-c", &cmd]).output()
    }.chain_err(|| format!("Unable to create command '{}'", cmd))?;

    if !output.status.success() {
        // nssm always generates 2 bytes char point
        // need to remove all the '\0' bytes
        bail!(
            r#"{} {{ exit code: {}, stdout: "{}", stderr: "{}" }}"#,
            cmd,
            match output.status.code() {
                Some(code) => format!("{}", code),
                None => "NIL".to_owned(),
            },
            String::from_utf8_lossy(&remove_zeros(&output.stdout)).trim(),
            String::from_utf8_lossy(&remove_zeros(&output.stderr)).trim()
        );
    }

    Ok(output)
}

fn run_nssm_cmd(cmd: &str, file_config: &FileConfig) -> Result<Output> {
    run_cmd(&format!(
        "{} {}",
        file_config.nssm_path.to_string_lossy(),
        cmd
    ))
}

fn run_nssm_set_cmd(cmd: &str, file_config: &FileConfig) -> Result<Output> {
    run_nssm_cmd(&format!("set {}", cmd), file_config)
}

fn run_nssm_set_cmd_if_some<T>(
    service_name: &str,
    field_name: &str,
    param: &Option<T>,
    file_config: &FileConfig,
) -> Result<()>
where
    T: Display,
{
    if let Some(ref param) = *param {
        let param_cmd = &format!("{} {} {}", service_name, field_name, param);

        run_nssm_set_cmd(param_cmd, file_config).chain_service_msg(
            &format!(
                "Unable to set '{}' for",
                field_name
            ),
            service_name,
        )?;
    }

    Ok(())
}

fn run_nssm_status_cmd(cmd: &str, file_config: &FileConfig) -> Result<Output> {
    run_nssm_cmd(&format!("status {}", cmd), file_config)
}

fn run_nssm_status_cmd_extract_status(cmd: &str, file_config: &FileConfig) -> Result<String> {
    run_nssm_status_cmd(cmd, file_config).and_then(|output| {
        let stdout = remove_zeros(&output.stdout);

        let status = std::str::from_utf8(&stdout)
            .chain_err(|| {
                format!(
                    "Unable to get convert from utf8 '{:?}' into status string",
                    stdout
                )
            })?
            .trim()
            .to_owned();

        Ok(status)
    })
}

fn poll_service_status_until_empty(
    service_name: &str,
    file_config: &FileConfig,
    poll_interval: &Duration,
    poll_count: u64,
) -> Result<()> {

    let has_stopped = (0..poll_count).any(|_| {
        let has_stopped = run_nssm_status_cmd_extract_status(service_name, file_config)
            .map(|status| status != SERVICE_STOP_PENDING_STATUS)
            .unwrap_or(false);

        if !has_stopped {
            info!(
                "Service '{}' still in pending stop state, waiting for it to stop...",
                service_name
            );
            
            thread::sleep(poll_interval.clone());
        }

        has_stopped
    });


    if !has_stopped {
        bail!(
            "Unable to wait for service name '{}' to stop completely",
            service_name
        )
    }

    Ok(())
}

fn merge_other_conf<'a, F, R>(
    lhs: &'a Option<OtherConfig>,
    rhs: &'a Option<OtherConfig>,
    chooser: F,
) -> Option<&'a R>
where
    F: Fn(&'a OtherConfig) -> Option<&'a R>,
{
    lhs.as_ref().and_then(&chooser).or(rhs.as_ref().and_then(
        &chooser,
    ))
}

fn remove_zeros(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .filter(|c| **c != 0)
        .map(|c| c.clone())
        .collect()
}

fn nssm_exec(file_config: &FileConfig) -> Result<()> {
    let pending_stop_poll_interval =
        Duration::from_millis(file_config.pending_stop_poll_ms.unwrap_or(
            PENDING_STOP_POLL_MS_DEF,
        ));

    let pending_stop_poll_count = file_config.pending_stop_poll_count.unwrap_or(
        PENDING_STOP_POLL_COUNT_DEF,
    );

    let log_names = file_config
        .services
        .iter()
        .map(|service| -> Result<()> {
            info!("Creating service '{}'...", service.name);

            // ignore if cannot get status, which probably means that the service does not exist yet
            if let Ok(status) = run_nssm_status_cmd_extract_status(&service.name, file_config) {
                debug!("Service '{}' exists, removing service...", service.name);

                if status != SERVICE_STOPPED_STATUS {
                    let stop_cmd = &format!("stop {}", service.name);

                    run_nssm_cmd(stop_cmd, file_config).chain_service_msg(
                        "Unable to stop",
                        &service.name,
                    )?;
                }

                // sometimes it takes a while to stop the service so wait for it
                poll_service_status_until_empty(
                    &service.name,
                    file_config,
                    &pending_stop_poll_interval,
                    pending_stop_poll_count,
                )?;

                let remove_cmd = &format!("remove {} confirm", service.name);

                run_nssm_cmd(remove_cmd, file_config).chain_service_msg(
                    "Unable to remove",
                    &service.name,
                )?;
            }

            // since nssm cannot use relative paths
            // must canonicalize the app path first
            let service_path_canon = service.path.canonicalize().chain_service_msg(
                &format!(
                    "Unable to canonicalize path '{}' for",
                    service.path.to_string_lossy()
                ),
                &service.name,
            )?;

            // install service first
            let install_cmd = &format!(
                "install {} {}",
                service.name,
                service_path_canon.to_string_lossy(),
            );

            run_nssm_cmd(install_cmd, file_config).chain_service_msg(
                "Unable to install",
                &service.name,
            )?;

            // then set the rest of the parameters
            if let Some(ref startup_dir) = service.startup_dir {
                // same for app directory
                let startup_dir_canon = startup_dir.canonicalize().chain_service_msg(
                    &format!(
                        "Unable to canonicalize startup directory path '{}' for",
                        startup_dir.to_string_lossy(),
                    ),
                    &service.name,
                )?;

                let app_dir_cmd = &format!(
                    "{} AppDirectory {}",
                    service.name,
                    startup_dir_canon.to_string_lossy()
                );

                run_nssm_set_cmd(app_dir_cmd, file_config)
                    .chain_service_msg("Unable to set startup directory for", &service.name)?;
            }

            run_nssm_set_cmd_if_some(&service.name, "AppParameters", &service.args, file_config)?;

            run_nssm_set_cmd_if_some(
                &service.name,
                "Description",
                &service.description,
                file_config,
            )?;

            // merges the options, prioritizing the local ones if available individually
            let merged_other = OtherConfigRef {
                deps: merge_other_conf(
                    &service.other,
                    &file_config.global,
                    |other| other.deps.as_ref(),
                ),
                start_on_create: merge_other_conf(&service.other, &file_config.global, |other| {
                    other.start_on_create.as_ref()
                }),
                account: merge_other_conf(&service.other, &file_config.global, |other| {
                    other.account.as_ref()
                }),
            };

            run_nssm_set_cmd_if_some(
                &service.name,
                "DependOnService",
                &merged_other.deps,
                file_config,
            )?;

            if let Some(account) = merged_other.account {
                let acct_cmd = &format!(
                    "{} ObjectName {} {}",
                    service.name,
                    account.user,
                    if !account.password.is_empty() {
                        &account.password
                    } else {
                        r#""""#
                    }
                );
                run_nssm_set_cmd(acct_cmd, file_config).chain_service_msg(
                    "Unable to set the username and password for",
                    &service.name,
                )?;
            }

            if let Some(start_on_create) = merged_other.start_on_create {
                if *start_on_create {
                    let start_cmd = &format!("start {}", service.name);

                    run_nssm_cmd(start_cmd, file_config).chain_service_msg(
                        "Unable to start",
                        &service.name,
                    )?;
                }
            }

            Ok(())
        })
        .zip(file_config.services.iter().map(|service| &service.name));

    // detailed logging
    for (log, name) in log_names {
        match log {
            Ok(_) => info!("Service '{}' [OK]", name),
            Err(e) => {
                error!("Service '{}' [FAILED]", name);
                print_recursive_err(&e);
            }
        }
    }

    Ok(())
}

fn run() -> Result<()> {
    let config = MainConfig::from_args();

    if let Some(ref log_config_path) = config.log_config_path {
        log4rs::init_file(log_config_path, Default::default())
            .chain_err(|| {
                format!(
                    "Unable to initialize log4rs logger with the given config file at '{}'",
                    log_config_path
                )
            })?;
    } else {
        simple_logger::init().chain_err(
            || "Unable to initialize default logger",
        )?;
    }

    let file_config_buf = file::get(&config.config_path).chain_err(|| {
        format!(
            "Unable to read TOML configuration file path at '{}'",
            config.config_path
        )
    })?;

    let file_config_str = String::from_utf8(file_config_buf).chain_err(
        || "Unable to convert TOML configuration file content into Rust String",
    )?;

    let file_config: FileConfig = toml::from_str(&file_config_str).chain_err(
        || "Unable to interpret configuration file content as TOML",
    )?;

    nssm_exec(&file_config).chain_err(
        || "Unable to complete all nssm operations",
    )?;

    Ok(())
}

fn print_recursive_err(e: &Error) {
    error!("ERROR: {}", e);

    for e in e.iter().skip(1) {
        error!("> Caused by: {}", e);
    }
}

fn main() {
    match run() {
        Ok(_) => {
            info!("Program completed!");
            process::exit(0)
        }

        Err(ref e) => {
            print_recursive_err(e);
            process::exit(1);
        }
    }
}
