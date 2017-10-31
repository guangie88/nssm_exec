#[macro_use]
extern crate derive_error_chain;
#[macro_use]
extern crate error_chain;
extern crate file;
extern crate itertools;
#[macro_use]
extern crate lazy_static;
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

use std::collections::HashMap;
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

    /// Interval in milliseconds before retrying to check if the service has started.
    /// Default is 500. Only applicable if there is any running existing service.
    pending_start_poll_ms: Option<u64>,

    /// Number of retries to check if the service has started.
    /// Default is 5. Only applicable if there is any running existing service.
    pending_start_poll_count: Option<u64>,

    /// Holds the global extra configurations.
    /// Any specific extra configurations will always override the global ones.
    global: Option<OtherConfig>,

    /// Holds the service configurations.
    services: Vec<Service>,
}

#[derive(StructOpt, Debug)]
#[structopt(name = "NSSM Executor")]
/// Program to facilitate easy adding of nssm services.
struct MainConfig {
    #[structopt(short = "c", long = "conf", default_value = "config/nssm_exec.toml")]
    /// TOML configuration to set up NSSM
    config_path: String,

    #[structopt(short = "l", long = "log", default_value = "config/logging_nssm_exec.yml")]
    /// Logging configuration file path
    log_config_path: Option<String>,

    #[structopt(subcommand)]
    /// Possible other specialized commands to use
    cmd: Option<CustomCmd>,
}

#[derive(StructOpt, Debug)]
enum CustomCmd {
    #[structopt(name = "stop")]
    /// Only runs the stop command on the services in the TOML configuration
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ServiceState {
    /// SERVICE_CONTINUE_PENDING (0x00000005)
    /// The service continue is pending.
    ContinuePending,

    /// SERVICE_PAUSE_PENDING (0x00000006)
    /// The service pause is pending.
    PausePending,

    /// SERVICE_PAUSED (0x00000007)
    /// The service is paused.
    Paused,

    /// SERVICE_RUNNING (0x00000004)
    /// The service is running.
    Running,

    /// SERVICE_START_PENDING (0x00000002)
    /// The service is starting.
    StartPending,

    /// SERVICE_STOP_PENDING (0x00000003)
    /// The service is stopping.
    StopPending,

    /// SERVICE_STOPPED (0x00000001)
    /// The service is not running.
    Stopped,
}

lazy_static! {
    static ref STATE_MAP: HashMap<&'static str, ServiceState> = {
        let mut m = HashMap::new();
        m.insert("SERVICE_CONTINUE_PENDING", ServiceState::ContinuePending);
        m.insert("SERVICE_PAUSE_PENDING", ServiceState::PausePending);
        m.insert("SERVICE_PAUSED", ServiceState::Paused);
        m.insert("SERVICE_RUNNING", ServiceState::Running);
        m.insert("SERVICE_START_PENDING", ServiceState::StartPending);
        m.insert("SERVICE_STOP_PENDING", ServiceState::StopPending);
        m.insert("SERVICE_STOPPED", ServiceState::Stopped);
        m
    };
}

mod errors {
    #[derive(Debug, ErrorChain)]
    pub enum ErrorKind {
        Msg(String),
    }
}

use errors::*;

const PENDING_POLL_DEFAULT_MS: u64 = 500;
const PENDING_POLL_DEFAULT_COUNT: u64 = 5;

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

fn state_from_str(status: &str) -> Result<ServiceState> {
    let state = STATE_MAP
        .get(status)
        .map(|state| state.clone())
        .ok_or_else(|| {
            format!(
                "Unable to obtain valid state from status string '{}'",
                status
            )
        })?;

    Ok(state)
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

fn run_nssm_status_cmd_extract_status(cmd: &str, file_config: &FileConfig) -> Result<ServiceState> {
    run_nssm_status_cmd(cmd, file_config).and_then(|output| {
        let stdout = remove_zeros(&output.stdout);

        let status = std::str::from_utf8(&stdout)
            .chain_err(|| {
                format!(
                    "Unable to get convert from utf8 '{:?}' into status string",
                    stdout
                )
            })?
            .trim();

        state_from_str(&status)
    })
}

fn poll_service_state_until(
    service_name: &str,
    file_config: &FileConfig,
    poll_interval: &Duration,
    poll_count: u64,
    expected_state: ServiceState,
) -> Result<()> {

    let status_check_iter = (0..poll_count).map(|_| {
        run_nssm_status_cmd_extract_status(service_name, file_config)
            .map(|status| status == expected_state)
            .unwrap_or(false)
    });

    // starts from 1 to reduce the count by 1 and prevent underflow
    let between_delay_iter = (1..poll_count).map(|_| {
        info!(
            "Service '{}' is still not in state {:?}, waiting...",
            service_name,
            expected_state
        );

        thread::sleep(poll_interval.clone());
        false
    });

    let state_reached = itertools::interleave(status_check_iter, between_delay_iter)
        .any(|reached| reached);

    if !state_reached {
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
        .filter(|&c| *c != 0)
        .map(|c| c.clone())
        .collect()
}

fn nssm_exec(file_config: &FileConfig) -> Result<()> {
    let pending_stop_poll_interval =
        Duration::from_millis(file_config.pending_stop_poll_ms.unwrap_or(
            PENDING_POLL_DEFAULT_MS,
        ));

    let pending_stop_poll_count = file_config.pending_stop_poll_count.unwrap_or(
        PENDING_POLL_DEFAULT_COUNT,
    );

    let pending_start_poll_interval =
        Duration::from_millis(file_config.pending_start_poll_ms.unwrap_or(
            PENDING_POLL_DEFAULT_MS,
        ));

    let pending_start_poll_count = file_config.pending_start_poll_count.unwrap_or(
        PENDING_POLL_DEFAULT_COUNT,
    );

    let log_names = file_config
        .services
        .iter()
        .map(|service| -> Result<()> {
            info!("Creating service '{}'...", service.name);

            // ignore if cannot get status, which probably means that the service does not exist yet
            if let Ok(state) = run_nssm_status_cmd_extract_status(&service.name, file_config) {
                debug!("Service '{}' exists, removing service...", service.name);

                if state != ServiceState::Stopped {
                    let stop_cmd = &format!("stop {}", service.name);

                    // sometimes the error message happens
                    // "Unexpected status SERVICE_STOP_PENDING in response to STOP control"
                    // even though the service will eventually stop
                    // so allow for this to happen

                    let stop_res = run_nssm_cmd(stop_cmd, file_config).chain_service_msg(
                        "Service stopping returned error, temporarily allowing this for",
                        &service.name,
                    );

                    if let Err(e) = stop_res {
                        print_recursive_warning(&e);
                    }

                    // sometimes it takes a while to stop the service so wait for it
                    poll_service_state_until(
                        &service.name,
                        file_config,
                        &pending_stop_poll_interval,
                        pending_stop_poll_count,
                        ServiceState::Stopped,
                    )?;
                }

                let remove_cmd = &format!("remove {} confirm", service.name);

                run_nssm_cmd(remove_cmd, file_config).chain_service_msg(
                    "Unable to remove",
                    &service.name,
                )?;
            }

            // install service first
            // note that the service path is relative from nssm.exe
            let install_cmd = &format!(
                "install {} {}",
                service.name,
                service.path.to_string_lossy(),
            );

            run_nssm_cmd(install_cmd, file_config).chain_service_msg(
                "Unable to install",
                &service.name,
            )?;

            // then set the rest of the parameters
            if let Some(ref startup_dir) = service.startup_dir {
                // app directory is also relative from nssm.exe
                let app_dir_cmd = &format!(
                    "{} AppDirectory {}",
                    service.name,
                    startup_dir.to_string_lossy()
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

            if let Some(&true) = merged_other.start_on_create {
                let start_cmd = &format!("start {}", service.name);

                let start_res = run_nssm_cmd(start_cmd, file_config).chain_service_msg(
                    "Service starting returned error, temporarily allowing this for",
                    &service.name,
                );

                if let Err(e) = start_res {
                    print_recursive_warning(&e);
                }

                // may take some time to start the service
                poll_service_state_until(
                    &service.name,
                    file_config,
                    &pending_start_poll_interval,
                    pending_start_poll_count,
                    ServiceState::Running,
                )?;
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

fn print_recursive_warning(e: &Error) {
    warn!("WARNING: {}", e);

    for e in e.iter().skip(1) {
        warn!("> Caused by: {}", e);
    }
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
