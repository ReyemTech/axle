//! Client commands that run on the user's machine (`axl login`, `axl logout`).

pub mod login;
pub mod service;

pub use login::{LoginArgs, LoginHooks, LoginOutcome, login, login_with, logout};
pub use service::{ServiceAction, run as service};
