use crate::models::{ActivitySummary, ProviderKind, UsageData};
use crate::{codex_poller, opencode_poller, poller};

#[derive(Debug)]
pub enum ProviderPollError {
    Unavailable,
    Loading,
}

pub struct ProviderPollResult {
    pub provider: ProviderKind,
    pub data: Result<UsageData, ProviderPollError>,
    pub activity: Option<ActivitySummary>,
}

pub fn poll_provider(provider: ProviderKind) -> ProviderPollResult {
    match provider {
        ProviderKind::Claude => ProviderPollResult {
            provider,
            data: poller::poll().map_err(|error| match error {
                poller::PollError::NoCredentials | poller::PollError::TokenExpired => {
                    ProviderPollError::Unavailable
                }
                poller::PollError::RequestFailed => ProviderPollError::Loading,
            }),
            activity: None,
        },
        ProviderKind::Codex => {
            let activity = codex_poller::read_activity_summary();
            ProviderPollResult {
                provider,
                data: codex_poller::poll().map_err(|error| match error {
                    codex_poller::PollError::NoCredentials
                    | codex_poller::PollError::CliUnavailable => ProviderPollError::Unavailable,
                    codex_poller::PollError::RequestFailed => ProviderPollError::Loading,
                }),
                activity,
            }
        }
        ProviderKind::OpenCode => ProviderPollResult {
            provider,
            data: opencode_poller::poll().map_err(|error| match error {
                opencode_poller::PollError::NoDatabase
                | opencode_poller::PollError::UnsupportedSchema => ProviderPollError::Unavailable,
                opencode_poller::PollError::QueryFailed => ProviderPollError::Loading,
            }),
            activity: None,
        },
    }
}
