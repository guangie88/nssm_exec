# nssm_exec
Utility wrapper program around [`nssm`](https://nssm.cc/) to run nssm commands to add services more easily, based on predefined TOML configuration. This allows for rapid stopping and recreating of predefined services using `nssm` via command line, which could be useful development and deployment.

[![Build status](https://ci.appveyor.com/api/projects/status/j90hd2yis46tcw31/branch/master?svg=true)](https://ci.appveyor.com/project/guangie88/nssm-exec/branch/master)

## How to Build
`cargo build` for Debug build, `cargo build --release` for Release build.

## How to Run
Assuming `config\nssm_exec.toml`, `third-party\five_ctrl_c.exe` and `nssm.exe` are present, running `cargo run --release` or `target\release\nssm_exec.exe` will automatically use the default configuration to demonstration a dummy set-up. Administrator rights are required since this involves installing of Windows services.

For a more practical set-up, the `config\nssm_exec.toml` file must be reconfigured.

For more arguments help, run `target\release\nssm_exec.exe --help`. Note that the program has an additional subcommand `stop` to perform only stopping of the services listed in the TOML configuration.

## TOML Example Configuration
The configuration file ([`config\nssm_exec.toml`](https://github.com/guangie88/nssm_exec/blob/master/config/nssm_exec.toml)) has the entire Rust data structures with comments to describe what each field does and whether it is optional.