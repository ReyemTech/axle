//! The human's actions (spec §12): every POST has passed the session's CSRF check in
//! `require_session`, runs one service call as `Actor::Human` (which records a task event or
//! a message and publishes it like an agent's write) and answers with a 303 back to a page.

use axum::Form;
use axum::extract::{Path, RawForm, State};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::views::{UiError, UiResult};
use crate::domain::{
    Actor, Address, Importance, MessageId, ProjectId, TaskId, TokenId, clean_display_name,
};
use crate::http::AppState;
use crate::service::auth::AuthError;
use crate::service::messages::{MessageError, NewMessage};
use crate::service::tasks::{Caller, NewTask, TaskError};
use crate::store::StoreError;

fn see_other(to: &str) -> Response {
    Redirect::to(to).into_response()
}

fn message_error(e: MessageError) -> UiError {
    match e {
        MessageError::BodyTooLarge { limit } => {
            UiError::Invalid(format!("The message is over {limit} bytes."))
        }
        e @ MessageError::TooLong { .. } => UiError::Invalid(capitalise(&e.to_string())),
        MessageError::NotFound(what) => UiError::Invalid(format!("That {what} does not exist.")),
        MessageError::Store(e) => e.into(),
    }
}

fn task_error(e: TaskError) -> UiError {
    match e {
        TaskError::NotFound => UiError::NotFound("No such task."),
        TaskError::Message(e) => message_error(e),
        TaskError::Store(e) => e.into(),
        e @ (TaskError::InvalidTransition { .. }
        | TaskError::Closed(_)
        | TaskError::NotOwner
        | TaskError::LeaseLost
        | TaskError::PlanNotApproved(_)
        | TaskError::NotPlanning(_)
        | TaskError::NoPlanToReview(_)) => UiError::Conflict(capitalise(&e.to_string())),
        e @ (TaskError::ReasonRequired
        | TaskError::EmptyPlan
        | TaskError::NoteRequired
        | TaskError::UnknownProject
        | TaskError::InvalidLeaseTtl
        | TaskError::ChildExtraProjects
        | TaskError::ChildProjectMismatch
        | TaskError::InvalidField { .. }) => UiError::Invalid(capitalise(&e.to_string())),
    }
}

/// Service messages are lower-case fragments; pages show sentences.
fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => format!("{}{}.", c.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

fn task_id(raw: &str) -> UiResult<TaskId> {
    raw.parse().map_err(|_| UiError::NotFound("No such task."))
}

/// An optional id field: blank is `None`, garbage is a 422.
fn opt_id<T: std::str::FromStr>(raw: &str, what: &str) -> UiResult<Option<T>> {
    match raw.trim() {
        "" => Ok(None),
        s => s
            .parse()
            .map(Some)
            .map_err(|_| UiError::Invalid(format!("Bad {what}."))),
    }
}

fn importance(raw: &str) -> UiResult<Importance> {
    match raw.trim() {
        "" => Ok(Importance::Normal),
        s => s
            .parse()
            .map_err(|_| UiError::Invalid("Importance is normal, high or urgent.".into())),
    }
}

fn required_body(body: &str) -> UiResult<()> {
    if body.trim().is_empty() {
        return Err(UiError::Invalid("A message needs some text.".into()));
    }
    Ok(())
}

fn subject(raw: &str) -> Option<String> {
    Some(raw.trim().to_string()).filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------- tasks

/// The new-task form: `project` (the lead), repeated `also` (other participating projects,
/// top-level tasks only), `plan_first` (a ticked checkbox sends `on`), `title`, `body` and
/// `parent`. Parsed by hand because a checkbox list repeats its field name.
#[derive(Default)]
struct CreateTaskForm {
    project: String,
    also: Vec<String>,
    plan_first: bool,
    title: String,
    body: String,
    parent: String,
}

impl CreateTaskForm {
    fn parse(raw: &[u8]) -> UiResult<Self> {
        let fields: Vec<(String, String)> =
            serde_urlencoded::from_bytes(raw).map_err(|_| UiError::Invalid("Bad form.".into()))?;
        let mut form = CreateTaskForm::default();
        for (k, v) in fields {
            match k.as_str() {
                "project" => form.project = v,
                "also" => form.also.push(v),
                "plan_first" => form.plan_first = v == "on",
                "title" => form.title = v,
                "body" => form.body = v,
                "parent" => form.parent = v,
                _ => {}
            }
        }
        Ok(form)
    }
}

pub async fn create_task(
    State(state): State<AppState>,
    RawForm(raw): RawForm,
) -> UiResult<Response> {
    let form = CreateTaskForm::parse(&raw)?;
    let title = form.title.trim();
    if title.is_empty() {
        return Err(UiError::Invalid("A task needs a title.".into()));
    }
    let project: ProjectId = opt_id(&form.project, "project")?
        .ok_or_else(|| UiError::Invalid("Pick a project.".into()))?;
    if state.store.get_project(project).await?.is_none() {
        return Err(UiError::Invalid("That project does not exist.".into()));
    }
    let parent = opt_id::<TaskId>(&form.parent, "parent task")?;
    let mut extra_projects = Vec::new();
    for raw in &form.also {
        let Some(id) = opt_id::<ProjectId>(raw, "project")? else {
            continue;
        };
        if state.store.get_project(id).await?.is_none() {
            return Err(UiError::Invalid("That project does not exist.".into()));
        }
        if id != project && !extra_projects.contains(&id) {
            extra_projects.push(id);
        }
    }
    let task = state
        .tasks
        .create(
            Caller::Human,
            NewTask {
                project_id: project,
                parent_id: parent,
                title: title.to_string(),
                body: form.body,
                extra_projects,
                requires_plan: form.plan_first,
                ..NewTask::default()
            },
        )
        .await
        .map_err(task_error)?;
    Ok(see_other(&format!("/ui/tasks/{}", task.id)))
}

#[derive(Deserialize)]
pub struct DecisionForm {
    #[serde(default)]
    note: String,
    /// The page to return to: the board or the overview (anything else: the task page).
    #[serde(default)]
    back: String,
}

/// Where a decision returns to: one of the pages that show "Needs you", else the task.
fn back_to(back: &str, id: TaskId) -> String {
    match back {
        "/ui/board" => "/ui/board#needs-you".into(),
        "/ui/" => "/ui/#needs-you".into(),
        _ => format!("/ui/tasks/{id}"),
    }
}

/// The human approves the submitted plan; the task becomes `claimed`.
pub async fn approve_plan(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<DecisionForm>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    state.tasks.approve_plan(id).await.map_err(task_error)?;
    Ok(see_other(&back_to(&form.back, id)))
}

/// The human asks for changes to the submitted plan (a note is required).
pub async fn request_plan_changes(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<DecisionForm>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    state
        .tasks
        .request_plan_changes(id, &form.note)
        .await
        .map_err(task_error)?;
    Ok(see_other(&back_to(&form.back, id)))
}

#[derive(Deserialize)]
pub struct AddProjectForm {
    #[serde(default)]
    project: String,
}

/// Adds a participating project to a top-level task.
pub async fn add_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<AddProjectForm>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    let project: ProjectId = opt_id(&form.project, "project")?
        .ok_or_else(|| UiError::Invalid("Pick a project.".into()))?;
    state
        .tasks
        .add_projects(Caller::Human, id, &[project])
        .await
        .map_err(task_error)?;
    Ok(see_other(&format!("/ui/tasks/{id}")))
}

#[derive(Deserialize)]
pub struct PostForm {
    #[serde(default)]
    body: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    importance: String,
}

/// Posts into the task's thread as the human.
pub async fn post_to_thread(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<PostForm>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    required_body(&form.body)?;
    if state.store.get_task(id).await?.is_none() {
        return Err(UiError::NotFound("No such task."));
    }
    state
        .messages
        .send(
            &Actor::Human,
            NewMessage {
                to: Address::Task(id),
                subject: subject(&form.subject),
                body: form.body,
                importance: importance(&form.importance)?,
                reply_to: None,
            },
        )
        .await
        .map_err(message_error)?;
    Ok(see_other(&format!("/ui/tasks/{id}")))
}

pub async fn force_release(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    state.tasks.force_release(id).await.map_err(task_error)?;
    Ok(see_other(&format!("/ui/tasks/{id}")))
}

pub async fn cancel(State(state): State<AppState>, Path(id): Path<String>) -> UiResult<Response> {
    let id = task_id(&id)?;
    state
        .tasks
        .cancel(Caller::Human, id)
        .await
        .map_err(task_error)?;
    Ok(see_other(&format!("/ui/tasks/{id}")))
}

// ---------------------------------------------------------------- messages

#[derive(Deserialize)]
pub struct MessageForm {
    /// `project`, `agent` or `task`; may instead prefix `to` as `kind:id`.
    #[serde(default)]
    to_kind: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    importance: String,
    #[serde(default)]
    reply_to: String,
}

fn address(kind: &str, id: &str) -> UiResult<Address> {
    let bad = || UiError::Invalid("Pick who the message is for.".into());
    let id: uuid::Uuid = id.trim().parse().map_err(|_| bad())?;
    Ok(match kind.trim() {
        "project" => Address::Project(id.into()),
        "agent" => Address::Agent(id.into()),
        "task" => Address::Task(id.into()),
        _ => return Err(bad()),
    })
}

/// Sends a message as the human to a project, an agent or a task thread.
pub async fn send_message(
    State(state): State<AppState>,
    Form(form): Form<MessageForm>,
) -> UiResult<Response> {
    required_body(&form.body)?;
    let to = match form.to_kind.trim() {
        "" => {
            let (kind, id) = form.to.split_once(':').unwrap_or(("", ""));
            address(kind, id)?
        }
        kind => address(kind, &form.to)?,
    };
    let reply_to = opt_id::<MessageId>(&form.reply_to, "message id")?;
    let sent = state
        .messages
        .send(
            &Actor::Human,
            NewMessage {
                to,
                subject: subject(&form.subject),
                body: form.body,
                importance: importance(&form.importance)?,
                reply_to,
            },
        )
        .await
        .map_err(message_error)?;
    Ok(see_other(&match reply_to {
        Some(original) => format!("/ui/inbox?m={original}"),
        None => format!("/ui/inbox?view=all&m={}", sent.id),
    }))
}

/// Acks a message in the human inbox ("archive" hides it; nothing is deleted).
pub async fn archive(State(state): State<AppState>, Path(id): Path<String>) -> UiResult<Response> {
    let id: MessageId = id
        .parse()
        .map_err(|_| UiError::NotFound("No such message."))?;
    state
        .messages
        .ack(Caller::Human, &[id])
        .await
        .map_err(message_error)?;
    Ok(see_other("/ui/inbox"))
}

// ---------------------------------------------------------------- projects and tokens

#[derive(Deserialize)]
pub struct RenameForm {
    #[serde(default)]
    display_name: String,
}

pub async fn rename_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<RenameForm>,
) -> UiResult<Response> {
    let id: ProjectId = id
        .parse()
        .map_err(|_| UiError::NotFound("No such project."))?;
    let name = clean_display_name(&form.display_name).map_err(|e| UiError::Invalid(e.into()))?;
    match state.store.rename_project(id, &name).await {
        Ok(()) => Ok(see_other("/ui/projects")),
        Err(StoreError::NotFound(_)) => Err(UiError::NotFound("No such project.")),
        Err(e) => Err(e.into()),
    }
}

pub async fn revoke_token(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> UiResult<Response> {
    let id: TokenId = id
        .parse()
        .map_err(|_| UiError::NotFound("No such token."))?;
    match state.auth.revoke(id).await {
        Ok(()) => Ok(see_other("/ui/tokens")),
        Err(AuthError::Store(StoreError::NotFound(_))) => Err(UiError::NotFound("No such token.")),
        Err(e) => Err(UiError::Internal(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse_by_kind() {
        let id = uuid::Uuid::now_v7();
        assert_eq!(
            address("project", &id.to_string()).unwrap(),
            Address::Project(id.into())
        );
        assert!(address("session", &id.to_string()).is_err());
        assert!(address("agent", "nope").is_err());
        assert_eq!(capitalise("cannot move"), "Cannot move.");
    }
}
