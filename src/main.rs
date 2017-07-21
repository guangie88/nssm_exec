#[macro_use] extern crate error_chain;
#[macro_use] extern crate derive_error_chain;
extern crate file;
#[macro_use] extern crate log;
extern crate log4rs;
extern crate serde;
#[macro_use] extern crate serde_derive;
extern crate simple_logger;
extern crate structopt;
#[macro_use] extern crate structopt_derive;
extern crate toml;

use std::path::PathBuf;
use std::process::{self, Command, Output};
use structopt::StructOpt;

#[derive(Deserialize)]
struct Account {
    user: String,
    password: String,
}

#[derive(Deserialize)]
struct OtherConfig {
    deps: Option<String>,
    start_on_create: Option<bool>,
    account: Option<Account>,
}

struct OtherConfigRef<'a, 'b, 'c> {
    deps: Option<&'a String>,
    start_on_create: Option<&'b bool>,
    account: Option<&'c Account>,
}

#[derive(Deserialize)]
struct Service {
    name: String,
    path: PathBuf,
    args: Option<String>,
    description: Option<String>,
    other: Option<OtherConfig>,
}

#[derive(Deserialize)]
struct FileConfig {
    nssm_path: PathBuf,
    global: Option<OtherConfig>,
    services: Vec<Service>,
}

#[derive(StructOpt, Debug)]
#[structopt(name = "nssm Executor", about = "Program to facilitate easy adding of nssm services.")]
struct MainConfig {
    #[structopt(short = "c", long = "conf", help = "TOML configuration to set up nssm", default_value = "config/nssm_exec.toml")]
    config_path: String,

    #[structopt(short = "l", long = "log", help = "Logging configuration file path", default_value = "config/logging_nssm_exec.yml")]
    log_config_path: Option<String>,
}

mod errors {
    #[derive(Debug, error_chain)]
    pub enum ErrorKind {
        Msg(String)
    }
}

use errors::*;

fn merger<'a, F, R>(lhs: &'a Option<OtherConfig>, rhs: &'a Option<OtherConfig>, chooser: F) -> Option<&'a R>
where F: Fn(&'a OtherConfig) -> Option<&'a R> {
    lhs.as_ref().and_then(&chooser)
        .or(rhs.as_ref().and_then(&chooser))
}

fn remove_zeros(bytes: &[u8]) -> Vec<u8> {
    bytes.iter()
        .filter(|c| **c != 0)
        .map(|c| c.clone())
        .collect()
}

fn nssm_exec(file_config: &FileConfig) -> Result<()> {
    let create_cmd = |cmd: String| -> Result<Output> {
        debug!("{}", cmd);

        let output =
            if cfg!(target_os = "windows") {
                Command::new("cmd").args(&["/C", &cmd]).output()
            } else {
                Command::new("sh").args(&["-c", &cmd]).output()
            }
            .chain_err(|| format!("Unable to create command '{}'", cmd))?;

        if !output.status.success() {
            // nssm always generates 2 bytes char point
            // need to remove all the '\0' bytes
            bail!(r#"{} {{ exit code: {}, stdout: "{}", stderr: "{}" }}"#,
                cmd,
                match output.status.code() { Some(code) => format!("{}", code), None => "NIL".to_owned(), },
                String::from_utf8_lossy(&remove_zeros(&output.stdout)).trim(),
                String::from_utf8_lossy(&remove_zeros(&output.stderr)).trim());
        }

        Ok(output)
    };

    let create_nssm_cmd = |cmd| {
        create_cmd(format!("{} {}",
            file_config.nssm_path.to_string_lossy(),
            cmd))
    };

    let create_nssm_status_cmd = |cmd| {
        create_nssm_cmd(format!("status {}", cmd))
    };

    let create_nssm_set_cmd = |cmd| {
        create_nssm_cmd(format!("set {}", cmd))
    };
    
    let log_names = file_config.services.iter()
        .map(|service| -> Result<()> {
            const SERVICE_STOPPED_STATUS: &str = "SERVICE_STOPPED";
            info!("Creating service '{}'...", service.name);

            let status_cmd = format!("{}", service.name);

            // ignore if cannot get status, which probably means that the service does not exist yet
            if let Ok(output) = create_nssm_status_cmd(status_cmd) {
                debug!("Service '{}' exists, removing service...", service.name);

                let stdout = remove_zeros(&output.stdout);
                let stdout = std::str::from_utf8(&stdout)
                    .chain_err(|| "Unable to get convert from utf8 into status string")?
                    .trim();

                if stdout != SERVICE_STOPPED_STATUS {
                    let stop_cmd = format!("stop {}", service.name);
                    create_nssm_cmd(stop_cmd).chain_err(|| "Unable to stop service")?;
                }

                let remove_cmd = format!("remove {} confirm", service.name);
                create_nssm_cmd(remove_cmd).chain_err(|| "Unable to remove service")?;
            }

            // install service first
            let install_cmd = format!(r#"install {} "{}""#, service.name, service.path.to_string_lossy());
            create_nssm_cmd(install_cmd).chain_err(|| "Unable to install service")?;

            // then set the rest of the parameters
            if let &Some(ref args) = &service.args {
                let params_cmd = format!("{} AppParameters {}", service.name, args);
                create_nssm_set_cmd(params_cmd).chain_err(|| "Unable to set arguments to service")?;
            }
            
            if let &Some(ref description) = &service.description {
                let description_cmd = format!("{} Description {}", service.name, description);
                create_nssm_set_cmd(description_cmd).chain_err(|| "Unable to set description to service")?;
            }

            // merges the options, prioritizing the local ones if available individually
            let merged_other = OtherConfigRef {
                deps: merger(&service.other, &file_config.global, |other| other.deps.as_ref()),
                start_on_create: merger(&service.other, &file_config.global, |other| other.start_on_create.as_ref()),
                account: merger(&service.other, &file_config.global, |other| other.account.as_ref()),
            };

            if let &Some(deps) = &merged_other.deps {
                let deps_cmd = format!("{} DependOnService {}", service.name, deps);
                create_nssm_set_cmd(deps_cmd).chain_err(|| "Unable to set dependencies to service")?;
            };

            if let &Some(account) = &merged_other.account {
                let acct_cmd = format!("{} ObjectName {} {}", service.name, account.user, if !account.password.is_empty() { &account.password } else { r#""""# });
                create_nssm_set_cmd(acct_cmd).chain_err(|| "Unable to set the username and password to service")?;
            }

            if let &Some(start_on_create) = &merged_other.start_on_create {
                if *start_on_create {
                    let start_cmd = format!("start {}", service.name);
                    create_nssm_cmd(start_cmd).chain_err(|| "Unable to start service")?;
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
            },
        }
    }

    Ok(())
}

fn run() -> Result<()> {
    let config = MainConfig::from_args();

    if let &Some(ref log_config_path) = &config.log_config_path {
        log4rs::init_file(log_config_path, Default::default())
            .chain_err(|| format!("Unable to initialize log4rs logger with the given config file at '{}'", log_config_path))?;
    } else {
        simple_logger::init()
            .chain_err(|| "Unable to initialize default logger")?;
    }

    let file_config_buf = file::get(&config.config_path)
        .chain_err(|| format!("Unable to read TOML configuration file path at '{}'", config.config_path))?;

    let file_config_str = String::from_utf8(file_config_buf)
        .chain_err(|| "Unable to convert TOML configuration file content into Rust String")?;

    let file_config: FileConfig = toml::from_str(&file_config_str)
        .chain_err(|| "Unable to interpret configuration file content as TOML")?;

    nssm_exec(&file_config)
        .chain_err(|| "Unable to complete all nssm operations")?;

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
        },

        Err(ref e) => {
            print_recursive_err(e);
            process::exit(1);
        },
    }
}