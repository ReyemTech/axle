//! Client commands that run on the user's machine (`axl install`, `axl login`, `axl logout`,
//! `axl service`).

pub mod install;
pub mod login;
pub mod service;
pub mod status;
pub mod uninstall;

pub use install::{
    HarnessRow, InstallArgs, InstallEnv, InstallReport, Outcome, Prompt, ServiceFactory,
    TerminalPrompt, install,
};
pub use login::{LoginArgs, LoginHooks, LoginOutcome, login, login_with, logout};
pub use service::{ServiceAction, run as service};
pub use status::{StatusLine, StatusReport, status, status_with};
pub use uninstall::{UninstallArgs, uninstall};
