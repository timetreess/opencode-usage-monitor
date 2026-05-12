#![windows_subsystem = "windows"]

mod codex_poller;
mod diagnose;
mod history;
mod localization;
mod models;
mod native_interop;
mod opencode_poller;
mod poller;
mod pricing;
mod provider_adapter;
mod secret_store;
mod theme;
mod updater;
mod window;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let diagnose_enabled = args.iter().any(|arg| arg == "--diagnose");
    if diagnose_enabled {
        match diagnose::init() {
            Ok(path) => diagnose::log(format!("startup args={args:?} log_path={}", path.display())),
            Err(error) => {
                // Logging may not be available yet, but keep startup behavior unchanged.
                let _ = error;
            }
        }
    }

    if let Some(exit_code) = updater::handle_cli_mode(&args) {
        if diagnose_enabled {
            diagnose::log(format!("cli mode exited with code {exit_code}"));
        }
        std::process::exit(exit_code);
    }

    if args.iter().any(|arg| arg == "--diagnose-once") {
        if diagnose_enabled {
            diagnose::log("running one-shot OpenCode diagnostic poll");
        }

        let result = provider_adapter::poll_provider(models::ProviderKind::OpenCode);
        let exit_code = match result.data {
            Ok(data) => {
                if diagnose_enabled {
                    if let Some(windows) = data.opencode_windows.as_ref() {
                        diagnose::log(format!(
                            "one-shot OpenCode poll succeeded: 5h_total={} today_total={} month_total={}",
                            windows.five_hour.total_tokens(),
                            windows.today.total_tokens(),
                            windows.month.total_tokens()
                        ));
                    } else {
                        diagnose::log("one-shot OpenCode poll succeeded without token windows");
                    }
                }
                0
            }
            Err(error) => {
                if diagnose_enabled {
                    diagnose::log(format!("one-shot OpenCode poll failed: {error:?}"));
                }
                1
            }
        };
        std::process::exit(exit_code);
    }

    if diagnose_enabled {
        diagnose::log("entering window::run");
    }
    window::run();
}
