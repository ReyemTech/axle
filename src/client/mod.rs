//! Client-side state under `~/.axl/`: the server URL, the token, and (later) the
//! enrollment manifest. Everything takes the home directory as a parameter so tests
//! never touch the real one.

pub mod api;
pub mod config;

pub use api::{Api, DeviceStartReply, PollStatus, SessionState, WhoAmI};

pub use config::{
    AxlDir, ClientConfig, ClientError, DEFAULT_SERVER_URL, UsageConfig, load_config, read_token,
    remove_token, resolve_server_url, resolve_server_url_with_warning, save_config, usage_off,
    write_token,
};
